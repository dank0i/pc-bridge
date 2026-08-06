//! Microphone / webcam in-use sensors.
//!
//! Publishes "on"/"off" to the `mic` and `webcam` sensors when the aggregate
//! in-use state changes.
//! - Windows: CapabilityAccessManager consent store (`LastUsedTimeStop == 0`).
//! - Linux: best-effort - a process holding `/dev/video*` (webcam) or an ALSA
//!   capture substream in the RUNNING state (mic). PipeWire setups that keep the
//!   device open may over-report the mic; treat Linux mic as approximate.

use log::{debug, info};
use std::sync::Arc;
#[cfg(not(windows))]
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct CaptureSensor {
    state: Arc<AppState>,
}

impl CaptureSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        // Subscribe before the first await so a per-task cancel fired during the
        // startup config read isn't missed (parity with session.rs, which
        // subscribes before any await).
        let shutdown_rx = shutdown.subscribe();
        let (mic, webcam) = {
            let c = self.state.config.read().await;
            (c.features.mic, c.features.webcam)
        };

        #[cfg(windows)]
        self.run_event_driven(mic, webcam, shutdown, shutdown_rx)
            .await;
        #[cfg(not(windows))]
        {
            drop(shutdown);
            self.run_polling(mic, webcam, shutdown_rx).await;
        }
    }

    /// Scan the current mic/webcam in-use state (off the runtime, since the scans
    /// block) and publish only the values that changed.
    async fn scan_and_publish(
        &self,
        mic: bool,
        webcam: bool,
        prev_mic: &mut Option<&'static str>,
        prev_cam: &mut Option<&'static str>,
    ) {
        // Outer Option: is the feature enabled. Inner: could the probe tell.
        let (m, w) = tokio::task::spawn_blocking(move || {
            (
                if mic { Some(mic_in_use()) } else { None },
                if webcam { Some(webcam_in_use()) } else { None },
            )
        })
        .await
        .unwrap_or((None, None));

        // Dedup on the published string: it now has three values, not two.
        if let Some(v) = m {
            let value = bool_state(v);
            if *prev_mic != Some(value) {
                self.state.mqtt.publish_sensor("mic", value).await;
                *prev_mic = Some(value);
            }
        }
        if let Some(v) = w {
            let value = bool_state(v);
            if *prev_cam != Some(value) {
                self.state.mqtt.publish_sensor("webcam", value).await;
                *prev_cam = Some(value);
            }
        }
    }

    /// Non-Windows: poll every 5s (Linux has no cheap consent-store change signal;
    /// /proc has to be walked).
    #[cfg(not(windows))]
    async fn run_polling(
        self,
        mic: bool,
        webcam: bool,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) {
        let mut tick = interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev_mic: Option<&'static str> = None;
        let mut prev_cam: Option<&'static str> = None;

        info!("Capture (mic/webcam) sensor started (polled every 5s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Capture sensor shutting down");
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
                        break;
                    }
                    prev_mic = None;
                    prev_cam = None;
                }
                _ = tick.tick() => {
                    self.scan_and_publish(mic, webcam, &mut prev_mic, &mut prev_cam).await;
                }
            }
        }
    }

    /// Windows: event-driven via `RegNotifyChangeKeyValue` on the consent store.
    /// A dedicated thread blocks (0% CPU) on a registry-change event and pokes us
    /// to re-read, instead of walking the whole consent store on a timer.
    #[cfg(windows)]
    async fn run_event_driven(
        self,
        mic: bool,
        webcam: bool,
        shutdown: tokio::sync::broadcast::Sender<()>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use windows::Win32::System::Threading::{CreateEventW, SetEvent};
        use windows::core::PCWSTR;

        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

        // Manual-reset stop event, owned by a refcounted `StopEvent` shared with
        // the watch thread. Whoever drops last closes it, so no caller can signal
        // a handle another has already closed.
        let stop_event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(h) => h,
            Err(e) => {
                log::error!("Capture: CreateEventW (stop) failed: {e:?}");
                return;
            }
        };
        let stop = Arc::new(StopEvent(stop_event));

        let (poke_tx, mut poke_rx) = tokio::sync::mpsc::channel::<()>(4);
        if let Err(e) = std::thread::Builder::new()
            .name("capture-consent".into())
            .stack_size(256 * 1024)
            .spawn({
                let stop = Arc::clone(&stop);
                move || consent_watch_thread(stop, poke_tx)
            })
        {
            log::error!("Capture: failed to spawn consent watch thread: {e}");
            // `stop` drops here and closes the event.
            return;
        }

        // Signal the stop event exactly once - whichever of the inline shutdown
        // arm or the independent waiter fires first - so we never SetEvent a
        // handle the watch thread has already closed.
        let signaled = Arc::new(AtomicBool::new(false));

        // Independent shutdown waiter: signal the stop event so the watch thread
        // unblocks and exits even if this task's inline arm never runs (abort).
        {
            let mut wait_rx = shutdown.subscribe();
            let signaled = Arc::clone(&signaled);
            // Hold an Arc, not the raw isize: this task is the one SetEvent caller
            // that sat OUTSIDE the refcount, so "use-after-close is not
            // representable" was not actually true for it. Safe today only because
            // the runtime is current-thread; an Arc makes it true unconditionally.
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let _ = wait_rx.recv().await;
                if !signaled.swap(true, Ordering::SeqCst) {
                    unsafe {
                        let _ = SetEvent(stop.0);
                    }
                }
            });
        }

        info!("Capture (mic/webcam) sensor started (event-driven)");
        let mut prev_mic: Option<&'static str> = None;
        let mut prev_cam: Option<&'static str> = None;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Capture sensor shutting down");
                    if !signaled.swap(true, Ordering::SeqCst) {
                        unsafe {
                            // Refcounted handle, not a rebuilt raw one - see the
                            // note on the shutdown waiter above.
                            let _ = SetEvent(stop.0);
                        }
                    }
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
                        break;
                    }
                    // Force a republish: clear the dedup state and rescan now.
                    prev_mic = None;
                    prev_cam = None;
                    self.scan_and_publish(mic, webcam, &mut prev_mic, &mut prev_cam).await;
                }
                Some(()) = poke_rx.recv() => {
                    self.scan_and_publish(mic, webcam, &mut prev_mic, &mut prev_cam).await;
                }
            }
        }
    }
}

/// Shared owner for the stop event.
///
/// Previously the async task held only the raw `isize` and the watch thread
/// closed the object on every exit path, while the async side still called
/// `SetEvent` on it. The `signaled` AtomicBool only de-duplicated the two ASYNC
/// callers; it gave no ordering against the thread's own close. If the thread
/// exited early (`RegOpenKeyExW` failing on Server/LTSC/a fresh profile) the
/// async side then signalled a RECYCLED handle value for the rest of the
/// session, and this process churns handles constantly.
///
/// Refcounting it means the last holder closes it, so a use-after-close is not
/// representable.
#[cfg(windows)]
pub(crate) struct StopEvent(pub(crate) windows::Win32::Foundation::HANDLE);

// SAFETY: a Windows event HANDLE is usable from any thread; we only ever
// SetEvent / wait on it, and Drop runs exactly once when the last Arc goes.
#[cfg(windows)]
unsafe impl Send for StopEvent {}
#[cfg(windows)]
unsafe impl Sync for StopEvent {}

#[cfg(windows)]
impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Dedicated OS thread: block on a `RegNotifyChangeKeyValue` change event for the
/// CapabilityAccessManager consent store and poke the async task to re-read on any
/// change. Exits when the stop event (rebuilt from `stop_raw`) is signaled.
#[cfg(windows)]
fn consent_watch_thread(stop: std::sync::Arc<StopEvent>, poke_tx: tokio::sync::mpsc::Sender<()>) {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
        RegCloseKey, RegNotifyChangeKeyValue, RegOpenKeyExW,
    };
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};
    use windows::core::{PCWSTR, w};

    unsafe {
        // Borrowed from the shared owner; this thread must NOT close it.
        let stop = stop.0;

        // Watch the ConsentStore parent with a subtree notification, so one watch
        // covers both microphone and webcam (and every app nested under them).
        let mut hkey = HKEY::default();
        let path = w!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore"
        );
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, 0, KEY_NOTIFY, &raw mut hkey).is_err() {
            log::error!("Capture: failed to open ConsentStore key for notifications");
            return;
        }

        // Auto-reset change event: WaitForMultipleObjects consumes the signal, so
        // no manual reset is needed between arms.
        let change = match CreateEventW(None, false, false, PCWSTR::null()) {
            Ok(h) => h,
            Err(e) => {
                log::error!("Capture: CreateEventW (change) failed: {e:?}");
                let _ = RegCloseKey(hkey);
                return;
            }
        };

        // Prime an initial scan so the sensor publishes current state at startup.
        let _ = poke_tx.blocking_send(());

        let filter = REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET;
        loop {
            // Arm the one-shot notification, then block on it or the stop event.
            if RegNotifyChangeKeyValue(hkey, true, filter, change, true).is_err() {
                log::warn!("Capture: RegNotifyChangeKeyValue failed; stopping consent watch");
                break;
            }
            let handles = [change, stop];
            let signaled = WaitForMultipleObjects(&handles, false, INFINITE);
            if signaled == WAIT_OBJECT_0 {
                // Consent store changed - re-read the aggregate in-use state.
                if poke_tx.blocking_send(()).is_err() {
                    break; // async side gone
                }
            } else {
                // Stop event (WAIT_OBJECT_0 + 1) or a wait failure - exit.
                break;
            }
        }

        let _ = CloseHandle(change);
        let _ = RegCloseKey(hkey);
    }
}

/// `None` means the probe could not determine the state (no `/proc` on macOS,
/// `hidepid=2` hiding every process on Linux). Publishing a confident "off"
/// there is a lie the user cannot distinguish from a real "not in use".
///
/// "unknown", NOT "unavailable": these sensors carry an availability topic, so a
/// literal `unavailable` state is indistinguishable in HA from the bridge's LWT
/// firing - which would destroy the exact distinction this tri-state creates.
/// `unknown` is what HA reserves for "the producer is alive but cannot say".
fn bool_state(on: Option<bool>) -> &'static str {
    match on {
        Some(true) => "on",
        Some(false) => "off",
        None => crate::mqtt::SENTINEL_UNKNOWN,
    }
}

// ── Windows: consent store ─────────────────────────────────────────────────

#[cfg(windows)]
fn mic_in_use() -> Option<bool> {
    capability_in_use("microphone")
}

#[cfg(windows)]
fn webcam_in_use() -> Option<bool> {
    capability_in_use("webcam")
}

/// True if any app currently holds the given capability ("microphone"/"webcam").
#[cfg(windows)]
fn capability_in_use(capability: &str) -> Option<bool> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!(
        r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{capability}"
    );
    // None, not false: the ConsentStore key is absent on Server/LTSC and on a
    // fresh profile before any app has requested the capability. Reporting a
    // confident "off" there is the same lie the Linux twin was fixed for.
    let Ok(store) = hkcu.open_subkey_with_flags(&path, KEY_READ) else {
        return None;
    };
    for app in store.enum_keys().flatten() {
        let Ok(app_key) = store.open_subkey_with_flags(&app, KEY_READ) else {
            continue;
        };
        if app.eq_ignore_ascii_case("NonPackaged") {
            // Desktop (non-store) apps are nested one level deeper.
            for np in app_key.enum_keys().flatten() {
                if let Ok(np_key) = app_key.open_subkey_with_flags(&np, KEY_READ)
                    && key_in_use(&np_key)
                {
                    return Some(true);
                }
            }
        } else if key_in_use(&app_key) {
            return Some(true);
        }
    }
    Some(false)
}

/// `LastUsedTimeStop == 0` means the capability is in use at this moment.
#[cfg(windows)]
fn key_in_use(key: &winreg::RegKey) -> bool {
    key.get_value::<u64, _>("LastUsedTimeStop")
        .map(|v| v == 0)
        .unwrap_or(false)
}

// ── Linux: /proc heuristics ────────────────────────────────────────────────

#[cfg(unix)]
fn webcam_in_use() -> Option<bool> {
    // A webcam is in use if any process has a /dev/video* device open.
    // None when /proc is absent (macOS) or unreadable (hidepid=2).
    proc_has_open_path("/dev/video")
}

#[cfg(unix)]
fn mic_in_use() -> Option<bool> {
    // ALSA capture substreams report "RUNNING" in their status file while
    // recording. Approximate on PipeWire, which may keep the device open.
    use std::fs;
    let Ok(cards) = fs::read_dir("/proc/asound") else {
        return None; // no /proc/asound: cannot tell, do not claim "off"
    };
    for card in cards.flatten() {
        let Ok(pcms) = fs::read_dir(card.path()) else {
            continue;
        };
        for pcm in pcms.flatten() {
            let name = pcm.file_name();
            let name = name.to_string_lossy();
            // Capture PCM directories end in 'c' (e.g. "pcm0c").
            if !(name.starts_with("pcm") && name.ends_with('c')) {
                continue;
            }
            let Ok(subs) = fs::read_dir(pcm.path()) else {
                continue;
            };
            for sub in subs.flatten() {
                if let Ok(content) = fs::read_to_string(sub.path().join("status"))
                    && content.contains("RUNNING")
                {
                    return Some(true);
                }
            }
        }
    }
    Some(false)
}

/// `Some(true)` if any process has an open file descriptor whose target starts
/// with `prefix` (e.g. "/dev/video"). `None` when /proc cannot be read at all
/// (macOS has none; `hidepid=2` hides every process), because reporting a
/// definite "not in use" there is indistinguishable from a real negative.
#[cfg(unix)]
fn proc_has_open_path(prefix: &str) -> Option<bool> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return None;
    };
    // With hidepid=2 (or systemd ProtectProc=invisible), and in any ordinary
    // multi-user case, /proc itself reads fine but every foreign fd dir gives
    // EACCES. Falling through to `false` there is exactly the confident lie this
    // tri-state exists to avoid, so track whether we could open ANY fd dir.
    let mut pids_seen = 0usize;
    let mut fd_dirs_opened = 0usize;
    for entry in entries.flatten() {
        // Only numeric entries are PIDs; /proc also holds many non-PID files.
        if !entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        pids_seen += 1;
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        fd_dirs_opened += 1;
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path())
                && target.to_string_lossy().starts_with(prefix)
            {
                return Some(true);
            }
        }
    }
    // Under hidepid=2 / ProtectProc=invisible a process still sees its OWN pid and
    // can always open its own fd dir, so `fd_dirs_opened == 0` never happens and
    // the old guard was dead. What actually distinguishes "restricted" from
    // "genuinely idle" is seeing many pids but being able to inspect ~none of them.
    //
    // The threshold has to be RELATIVE. A fixed `fd_dirs_opened <= 1` only
    // catches total blindness: running as a dedicated service user, pc-bridge
    // typically owns two or three processes of its own, clears the fixed bar,
    // and then confidently answers "no, the camera is not in use" without having
    // been able to look at a single desktop-session process. Requiring a real
    // fraction of visible pids to be inspectable keeps the tri-state honest.
    if pids_seen > 4 && fd_dirs_opened * 5 < pids_seen {
        return None;
    }
    Some(false)
}
