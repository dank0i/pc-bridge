//! Now Playing (media session) sensor.
//!
//! Publishes "playing: Artist - Title" / "paused: ..." / "idle" to the
//! `now_playing` sensor.
//! - Windows: System Media Transport Controls (GSMTC), on a dedicated MTA
//!   thread so we init COM once, cache the session manager, and never land on
//!   an STA blocking-pool thread (where a WinRT async `.get()` would hang with
//!   no message pump).
//! - Linux: MPRIS via `playerctl`.

use log::{debug, info};
use std::sync::Arc;

use crate::AppState;

#[cfg(unix)]
static PLAYERCTL_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct NowPlayingSensor {
    state: Arc<AppState>,
}

impl NowPlayingSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    #[cfg(windows)]
    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut shutdown_rx = shutdown.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev = String::new();

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
        // Wake channel: GSMTC event handlers and the shutdown path both poke this,
        // so the media thread blocks (zero CPU) instead of slice-sleeping, and
        // shutdown is immediate rather than up to 100ms late.
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let thread_wake_tx = wake_tx.clone();
        let thread_stop = Arc::clone(&stop);
        if let Err(e) = std::thread::Builder::new()
            .name("now-playing".into())
            .stack_size(256 * 1024)
            .spawn(move || windows_media_loop(&thread_stop, &tx, &wake_rx, &thread_wake_tx))
        {
            log::error!("Failed to spawn now-playing thread: {e}");
            return;
        }

        info!("Now playing sensor started (Windows GSMTC, dedicated thread)");

        // Independent shutdown waiter: set the stop flag so the media thread exits
        // even if the supervisor aborts run() (which would skip the inline arm).
        {
            let mut wait_rx = shutdown.subscribe();
            let waiter_stop = Arc::clone(&stop);
            let waiter_wake = wake_tx.clone();
            tokio::spawn(async move {
                let _ = wait_rx.recv().await;
                waiter_stop.store(true, Ordering::Relaxed);
                // Poke the media thread so it leaves recv_timeout at once rather
                // than sitting out the backstop interval.
                let _ = waiter_wake.send(());
            });
        }

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Now playing sensor shutting down");
                    stop.store(true, Ordering::Relaxed);
                    // Poke the media thread out of recv_timeout immediately.
                    let _ = wake_tx.send(());
                    break;
                }
                r = reconnect_rx.recv() => {
                    // Treat Lagged as a reconnect: the `Ok(())` pattern alone
                    // silently skipped this arm when the 4-slot channel
                    // overran, losing the republish on a flapping broker.
                    // Closed means the sender is gone; `continue` would spin the
                    // loop hot on an instantly-ready recv(). Exit instead.
                    if !matches!(
                        r,
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) {
                        // Stop the media thread too. Exiting run() without this
                        // lets the supervisor respawn while the old thread is
                        // still alive, giving two. The thread is bounded here:
                        // it sits in wake_rx.recv_timeout(BACKSTOP) and sees
                        // `stop` within 60s at worst.
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    // Republish the CURRENT value, do not just clear the dedup.
                    // The media thread now dedups too, so after a broker restart
                    // that dropped retained state nothing would be resent until
                    // the media value happened to change - which on an idle
                    // machine is never.
                    if !prev.is_empty() {
                        self.state
                            .mqtt
                            .publish_sensor("now_playing", &prev)
                            .await;
                    }
                }
                r = rx.recv() => {
                    // None means the media thread died; exit so the supervisor
                    // sees a finished handle and respawns instead of leaving this
                    // task parked forever on a branch tokio has disabled.
                    let Some(now) = r else { break };
                    if now != prev {
                        self.state
                            .mqtt
                            .publish_sensor("now_playing", &now)
                            .await;
                        prev = now;
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut shutdown_rx = shutdown.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev = String::new();

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
        // Shared so shutdown can kill the child, unblocking the stdout read that
        // would otherwise sit there until playerctl next emits.
        let child: Arc<std::sync::Mutex<Option<std::process::Child>>> =
            Arc::new(std::sync::Mutex::new(None));

        let thread_stop = Arc::clone(&stop);
        let thread_child = Arc::clone(&child);
        if let Err(e) = std::thread::Builder::new()
            .name("now-playing".into())
            .stack_size(256 * 1024)
            .spawn(move || unix_media_loop(&thread_stop, &tx, &thread_child))
        {
            log::error!("Failed to spawn now-playing thread: {e}");
            return;
        }

        info!("Now playing sensor started (Linux playerctl --follow, event-driven)");

        // Independent shutdown waiter, matching the Windows branch: stops the
        // thread even if the supervisor aborts run() and skips the inline arm.
        {
            let mut wait_rx = shutdown.subscribe();
            let waiter_stop = Arc::clone(&stop);
            let waiter_child = Arc::clone(&child);
            tokio::spawn(async move {
                let _ = wait_rx.recv().await;
                waiter_stop.store(true, Ordering::Relaxed);
                kill_child(&waiter_child);
            });
        }

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Now playing sensor shutting down");
                    stop.store(true, Ordering::Relaxed);
                    kill_child(&child);
                    break;
                }
                r = reconnect_rx.recv() => {
                    // Treat Lagged as a reconnect; Closed means the sender is gone.
                    if !matches!(
                        r,
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) {
                        // Stop the media thread too. Exiting run() without this
                        // lets the supervisor respawn while the old thread is
                        // still alive, giving two.
                        //
                        // `stop` alone is NOT enough here: the thread blocks in
                        // lines() on playerctl's stdout and only checks `stop`
                        // BETWEEN lines, so a paused or idle player that emits
                        // nothing parks it indefinitely - and its playerctl child
                        // with it. Killing the child closes the pipe, which ends
                        // the iterator. The shutdown arm above already does this;
                        // this arm was missing it.
                        stop.store(true, Ordering::Relaxed);
                        kill_child(&child);
                        break;
                    }
                    // Republish the CURRENT value, do not just clear the dedup.
                    // The media thread now dedups too, so after a broker restart
                    // that dropped retained state nothing would be resent until
                    // the media value happened to change - which on an idle
                    // machine is never.
                    if !prev.is_empty() {
                        self.state
                            .mqtt
                            .publish_sensor("now_playing", &prev)
                            .await;
                    }
                }
                r = rx.recv() => {
                    // None means the media thread died; exit so the supervisor
                    // sees a finished handle and respawns instead of leaving this
                    // task parked forever on a branch tokio has disabled.
                    let Some(now) = r else { break };
                    if now != prev {
                        self.state
                            .mqtt
                            .publish_sensor("now_playing", &now)
                            .await;
                        prev = now;
                    }
                }
            }
        }
    }
}

/// Kill AND reap the child, so a respawn loop cannot accumulate zombies.
#[cfg(unix)]
fn reap(slot: &std::sync::Mutex<Option<std::process::Child>>) {
    if let Ok(mut guard) = slot.lock()
        && let Some(mut child) = guard.take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Kill the running `playerctl --follow` child, if any, so a blocked stdout read
/// returns at once.
#[cfg(unix)]
fn kill_child(slot: &std::sync::Mutex<Option<std::process::Child>>) {
    if let Ok(mut guard) = slot.lock()
        && let Some(child) = guard.as_mut()
    {
        let _ = child.kill();
    }
}

/// Dedicated-thread loop: stream `playerctl metadata --follow`, which emits one
/// line per change, instead of polling every 5s.
///
/// Parity with the Windows GSMTC branch: the OS (well, the player over MPRIS)
/// tells us when something changes rather than us asking 17,280 times a day.
///
/// `--follow` does NOT exit when the last player disappears: playerctl prints a
/// bare newline and keeps running, which `parse_playerctl` correctly reads as
/// `idle`. So the respawn path below is a recovery route for playerctl actually
/// dying (or never starting), not the normal no-players case.
///
/// The format string is `{{lc(status)}}\t{{artist}}\t{{title}}`, and the
/// `lc(status)` call is load-bearing beyond formatting: playerctl's key scan
/// recurses into function arguments, so wrapping `status` registers it as a
/// followed key and connects the playback-status signal. Without it every
/// play/pause transition would be silently dropped.
#[cfg(unix)]
fn unix_media_loop(
    stop: &std::sync::atomic::AtomicBool,
    tx: &tokio::sync::mpsc::Sender<String>,
    child_slot: &std::sync::Mutex<Option<std::process::Child>>,
) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::atomic::Ordering;

    /// First respawn delay after playerctl exits.
    const RESPAWN_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
    /// Cap for the escalating backoff. The realistic failure is playerctl
    /// installed but exiting immediately (headless service with no
    /// DBUS_SESSION_BUS_ADDRESS); without escalation that is a fork/exec every
    /// 5s forever, with stderr nulled, from code advertising itself as
    /// event-driven.
    const RESPAWN_CAP: std::time::Duration = std::time::Duration::from_secs(300);
    /// A missing binary will not appear on its own.
    const MISSING_DELAY: std::time::Duration = std::time::Duration::from_secs(300);

    let mut backoff = RESPAWN_DELAY;

    // Sleep in slices ONLY here, where there is no waitable object to block on.
    // The steady state is blocked on stdout, not spinning.
    let nap = |d: std::time::Duration| {
        let slices = d.as_millis() / 200;
        for _ in 0..slices {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    };

    while !stop.load(Ordering::Relaxed) {
        let spawned = Command::new("playerctl")
            .args([
                "metadata",
                "--follow",
                "--format",
                "{{lc(status)}}\t{{artist}}\t{{title}}",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let mut proc = match spawned {
            Ok(c) => c,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound
                    && !PLAYERCTL_WARNED.swap(true, Ordering::Relaxed)
                {
                    log::warn!(
                        "playerctl not found; now_playing will report 'unknown' \
                         (install playerctl)"
                    );
                }
                // "unknown", not "idle": with no playerctl we cannot observe
                // media at all, so "idle" is a fabrication. Permanent on macOS,
                // where this cfg(unix) branch also runs and playerctl is
                // essentially never present.
                let _ = tx.blocking_send(crate::mqtt::SENTINEL_UNKNOWN.to_string());
                nap(MISSING_DELAY);
                continue;
            }
        };

        let stdout = proc.stdout.take();
        if let Ok(mut guard) = child_slot.lock() {
            *guard = Some(proc);
        }
        // Re-check AFTER storing: a shutdown landing between spawn and store
        // would have found an empty slot, so the child would outlive the kill and
        // this thread would block in lines() with nothing left to interrupt it.
        if stop.load(Ordering::Relaxed) {
            reap(child_slot);
            break;
        }

        if let Some(out) = stdout {
            for line in BufReader::new(out).lines() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(line) = line else { break };
                if tx.blocking_send(parse_playerctl(&line)).is_err() {
                    // Reap before leaving, or the child is orphaned: Child::drop
                    // neither kills nor waits.
                    reap(child_slot);
                    return;
                }
            }
        }

        reap(child_slot);

        if stop.load(Ordering::Relaxed) {
            break;
        }
        // playerctl died or never started. "unknown", not "idle": the common
        // cause is playerctl present but exiting immediately because there is no
        // DBUS_SESSION_BUS_ADDRESS (headless service). A producer that knows it
        // cannot see anything must not assert that nothing is playing - that is
        // the same fabrication the missing-binary path above avoids. Back off,
        // doubling to the cap so a permanently broken environment stops forking.
        let _ = tx.blocking_send(crate::mqtt::SENTINEL_UNKNOWN.to_string());
        nap(backoff);
        backoff = (backoff * 2).min(RESPAWN_CAP);
    }
}

/// Dedicated-thread loop: init COM (MTA) once, request the GSMTC manager once,
/// then wait on GSMTC EVENTS rather than polling.
///
/// Previously this polled every 5s, sleeping in 100ms slices purely so the stop
/// flag was checked promptly: 864,000 thread wakes and 17,280 cross-process
/// WinRT round trips per day, whether or not anything was playing. Now the OS
/// tells us when something changes.
///
/// Three events matter, and the session-scoped two must be re-registered every
/// time the current session swaps:
/// * manager `CurrentSessionChanged` - a different app took over media control
/// * session `MediaPropertiesChanged` - track changed
/// * session `PlaybackInfoChanged`   - play/pause/stop
///
/// A slow backstop poll remains because some apps do not fire
/// `MediaPropertiesChanged` reliably. It is a safety net, not the mechanism.
///
/// Handlers run on a WinRT thread pool, so they only signal a channel; all
/// WinRT reads happen back on this thread.
#[cfg(windows)]
fn windows_media_loop(
    stop: &std::sync::atomic::AtomicBool,
    tx: &tokio::sync::mpsc::Sender<String>,
    wake_rx: &std::sync::mpsc::Receiver<()>,
    wake_tx: &std::sync::mpsc::Sender<()>,
) {
    use std::sync::atomic::Ordering;
    use windows::Foundation::TypedEventHandler;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSession as Session;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    /// Safety net for apps that do not raise MediaPropertiesChanged.
    const BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // Retried on every wake while still None. Acquiring it once meant a failure
    // at startup (agent up before the media stack, session 0, RDP) pinned
    // now_playing to "idle" for the whole process lifetime behind one debug line.
    let mut manager: Option<Manager> = None;

    // Session-scoped registrations, re-created on every session swap. Tokens MUST
    // be removed before dropping the session or the handler leaks and can fire
    // against a session we no longer track.
    let mut session: Option<Session> = None;
    // INDEPENDENT options, not a pair: if MediaPropertiesChanged succeeds and
    // PlaybackInfoChanged fails, a pair would leave BOTH untracked and leak a
    // live handler that keeps poking the wake channel for a session we no longer
    // follow.
    let mut props_token: Option<windows::Foundation::EventRegistrationToken> = None;
    let mut playback_token: Option<windows::Foundation::EventRegistrationToken> = None;

    let unhook_session =
        |sess: &mut Option<Session>,
         props: &mut Option<windows::Foundation::EventRegistrationToken>,
         playback: &mut Option<windows::Foundation::EventRegistrationToken>| {
            if let Some(s) = sess.as_ref() {
                if let Some(t) = props.take() {
                    let _ = s.RemoveMediaPropertiesChanged(t);
                }
                if let Some(t) = playback.take() {
                    let _ = s.RemovePlaybackInfoChanged(t);
                }
            }
            *sess = None;
        };

    let mut manager_token: Option<windows::Foundation::EventRegistrationToken> = None;
    // Set by the manager's CurrentSessionChanged handler. Rebinding is driven by
    // this flag rather than by comparing session objects: PartialEq on a WinRT
    // session does a QueryInterface that can PANIC on a disconnected proxy
    // (killing this thread silently), and nothing in the GSMTC contract promises
    // GetCurrentSession returns a cached instance, so a distinct object per call
    // would re-register on every wake. The event is the authoritative signal.
    let session_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    let mut last_sent: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Acquire (or re-acquire) the manager, then hook its session-changed event.
        if manager.is_none() {
            manager = Manager::RequestAsync().ok().and_then(|op| op.get().ok());
            if let Some(m) = manager.as_ref() {
                let waker = wake_tx.clone();
                let dirty = std::sync::Arc::clone(&session_dirty);
                manager_token = m
                    .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                        // Signal only: no WinRT work on the thread-pool thread.
                        dirty.store(true, Ordering::Relaxed);
                        let _ = waker.send(());
                        Ok(())
                    }))
                    .ok();
                session_dirty.store(true, Ordering::Relaxed);
            }
        }

        // Re-bind session-scoped handlers only when the event says to.
        if let Some(m) = manager.as_ref()
            && session_dirty.swap(false, Ordering::Relaxed)
        {
            unhook_session(&mut session, &mut props_token, &mut playback_token);
            if let Ok(sess) = m.GetCurrentSession() {
                let props_waker = wake_tx.clone();
                let playback_waker = wake_tx.clone();
                props_token = sess
                    .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                        let _ = props_waker.send(());
                        Ok(())
                    }))
                    .ok();
                playback_token = sess
                    .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                        let _ = playback_waker.send(());
                        Ok(())
                    }))
                    .ok();
                session = Some(sess);
            }
        }

        // Read from the session we just bound handlers to, rather than calling
        // GetCurrentSession again: that doubled the cross-process round trips per
        // wake and opened a window where handlers were bound to session A while
        // the published value came from session B.
        // "unknown" when we cannot observe the media stack at all, "idle" only
        // when we CAN observe it and there is genuinely no session. Publishing
        // "idle" for an unavailable manager is asserting something we know we do
        // not know.
        let value = match (manager.as_ref(), session.as_ref()) {
            (None, _) => crate::mqtt::SENTINEL_UNKNOWN.to_string(),
            // 3. Do not collapse every HRESULT to "idle": a failure reading a
            // LIVE session is not the same as there being no session.
            (Some(_), Some(s)) => {
                read_session_of(s).unwrap_or_else(|_| crate::mqtt::SENTINEL_UNKNOWN.to_string())
            }
            (Some(_), None) => "idle".to_string(),
        };
        if last_sent.as_deref() != Some(value.as_str()) {
            if tx.blocking_send(value.clone()).is_err() {
                break; // async side dropped
            }
            last_sent = Some(value);
        }

        // Blocks until an event fires, shutdown pokes us, or the backstop expires.
        // No slice-sleeping: shutdown is immediate because the waiter sends here.
        match wake_rx.recv_timeout(BACKSTOP) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // If CurrentSessionChanged never registered, that event is the ONLY
        // thing that sets session_dirty, so the sensor would stay bound to
        // whichever app owned media control at startup for the rest of the
        // process. Fall back to re-binding on the backstop instead.
        if manager_token.is_none() {
            session_dirty.store(true, Ordering::Relaxed);
        }
        // Coalesce a burst (a track change raises several events at once).
        while wake_rx.try_recv().is_ok() {}
    }

    // Unregister BEFORE dropping the COM objects, then release them before
    // CoUninitialize, or their Release runs on a dead apartment.
    unhook_session(&mut session, &mut props_token, &mut playback_token);
    if let (Some(m), Some(tok)) = (manager.as_ref(), manager_token) {
        let _ = m.RemoveCurrentSessionChanged(tok);
    }
    drop(session);
    drop(manager);

    unsafe {
        CoUninitialize();
    }
}

#[cfg(windows)]
fn read_session_of(
    session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession,
) -> windows_core::Result<String> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status;

    let status = session.GetPlaybackInfo()?.PlaybackStatus()?;
    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = props.Title()?.to_string();
    let artist = props.Artist()?.to_string();

    let label = match (artist.trim().is_empty(), title.trim().is_empty()) {
        (false, false) => format!("{} - {}", artist.trim(), title.trim()),
        (true, false) => title.trim().to_string(),
        (false, true) => artist.trim().to_string(),
        (true, true) => return Ok("idle".to_string()),
    };
    let prefix = if status == Status::Playing {
        "playing"
    } else if status == Status::Paused {
        "paused"
    } else {
        "stopped"
    };
    Ok(format!("{prefix}: {label}"))
}

/// Turn a tab-delimited "status\tartist\ttitle" line into a sensor value,
/// collapsing empty artist/title exactly like the Windows branch.
#[cfg(unix)]
fn parse_playerctl(raw: &str) -> String {
    let line = raw.trim_end_matches(['\n', '\r']);
    let mut parts = line.splitn(3, '\t');
    let status = parts.next().unwrap_or("").trim();
    let artist = parts.next().unwrap_or("").trim();
    let title = parts.next().unwrap_or("").trim();

    let label = match (artist.is_empty(), title.is_empty()) {
        (false, false) => format!("{artist} - {title}"),
        (true, false) => title.to_string(),
        (false, true) => artist.to_string(),
        (true, true) => return "idle".to_string(),
    };
    if status.is_empty() {
        "idle".to_string()
    } else {
        format!("{status}: {label}")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::parse_playerctl;

    #[test]
    fn test_parse_playerctl() {
        assert_eq!(
            parse_playerctl("playing\tQueen\tBohemian Rhapsody\n"),
            "playing: Queen - Bohemian Rhapsody"
        );
        // Empty artist / empty title collapse (no dangling " - ").
        assert_eq!(
            parse_playerctl("playing\t\tBohemian Rhapsody"),
            "playing: Bohemian Rhapsody"
        );
        assert_eq!(parse_playerctl("paused\tArtist\t"), "paused: Artist");
        // No metadata / no player -> idle.
        assert_eq!(parse_playerctl("playing\t\t"), "idle");
        assert_eq!(parse_playerctl(""), "idle");
    }
}
