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
        prev_mic: &mut Option<bool>,
        prev_cam: &mut Option<bool>,
    ) {
        let (m, w) =
            tokio::task::spawn_blocking(move || (mic.then(mic_in_use), webcam.then(webcam_in_use)))
                .await
                .unwrap_or((None, None));

        if let Some(on) = m
            && *prev_mic != Some(on)
        {
            self.state
                .mqtt
                .publish_sensor_retained("mic", bool_state(on))
                .await;
            *prev_mic = Some(on);
        }
        if let Some(on) = w
            && *prev_cam != Some(on)
        {
            self.state
                .mqtt
                .publish_sensor_retained("webcam", bool_state(on))
                .await;
            *prev_cam = Some(on);
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
        let mut prev_mic: Option<bool> = None;
        let mut prev_cam: Option<bool> = None;

        info!("Capture (mic/webcam) sensor started (polled every 5s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Capture sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
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

        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Threading::{CreateEventW, SetEvent};
        use windows::core::PCWSTR;

        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

        // Manual-reset stop event. We keep only its raw value (isize, Send) in the
        // async scope so the future stays Send, rebuilding the HANDLE for the
        // (non-await) SetEvent calls. The watch thread owns and closes the object.
        let stop_event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(h) => h,
            Err(e) => {
                log::error!("Capture: CreateEventW (stop) failed: {e:?}");
                return;
            }
        };
        let stop_raw = stop_event.0 as isize;

        let (poke_tx, mut poke_rx) = tokio::sync::mpsc::channel::<()>(4);
        if let Err(e) = std::thread::Builder::new()
            .name("capture-consent".into())
            .stack_size(256 * 1024)
            .spawn(move || consent_watch_thread(stop_raw, poke_tx))
        {
            log::error!("Capture: failed to spawn consent watch thread: {e}");
            unsafe {
                let _ = CloseHandle(stop_event);
            }
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
            tokio::spawn(async move {
                let _ = wait_rx.recv().await;
                if !signaled.swap(true, Ordering::SeqCst) {
                    unsafe {
                        let _ = SetEvent(HANDLE(stop_raw as *mut _));
                    }
                }
            });
        }

        info!("Capture (mic/webcam) sensor started (event-driven)");
        let mut prev_mic: Option<bool> = None;
        let mut prev_cam: Option<bool> = None;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Capture sensor shutting down");
                    if !signaled.swap(true, Ordering::SeqCst) {
                        unsafe {
                            let _ = SetEvent(HANDLE(stop_raw as *mut _));
                        }
                    }
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
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

/// Dedicated OS thread: block on a `RegNotifyChangeKeyValue` change event for the
/// CapabilityAccessManager consent store and poke the async task to re-read on any
/// change. Exits when the stop event (rebuilt from `stop_raw`) is signaled.
#[cfg(windows)]
fn consent_watch_thread(stop_raw: isize, poke_tx: tokio::sync::mpsc::Sender<()>) {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
        RegCloseKey, RegNotifyChangeKeyValue, RegOpenKeyExW,
    };
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};
    use windows::core::{PCWSTR, w};

    unsafe {
        let stop = HANDLE(stop_raw as *mut _);

        // Watch the ConsentStore parent with a subtree notification, so one watch
        // covers both microphone and webcam (and every app nested under them).
        let mut hkey = HKEY::default();
        let path = w!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore"
        );
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, 0, KEY_NOTIFY, &raw mut hkey).is_err() {
            log::error!("Capture: failed to open ConsentStore key for notifications");
            let _ = CloseHandle(stop);
            return;
        }

        // Auto-reset change event: WaitForMultipleObjects consumes the signal, so
        // no manual reset is needed between arms.
        let change = match CreateEventW(None, false, false, PCWSTR::null()) {
            Ok(h) => h,
            Err(e) => {
                log::error!("Capture: CreateEventW (change) failed: {e:?}");
                let _ = RegCloseKey(hkey);
                let _ = CloseHandle(stop);
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
        let _ = CloseHandle(stop);
    }
}

fn bool_state(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

// ── Windows: consent store ─────────────────────────────────────────────────

#[cfg(windows)]
fn mic_in_use() -> bool {
    capability_in_use("microphone")
}

#[cfg(windows)]
fn webcam_in_use() -> bool {
    capability_in_use("webcam")
}

/// True if any app currently holds the given capability ("microphone"/"webcam").
#[cfg(windows)]
fn capability_in_use(capability: &str) -> bool {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!(
        r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{capability}"
    );
    let Ok(store) = hkcu.open_subkey_with_flags(&path, KEY_READ) else {
        return false;
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
                    return true;
                }
            }
        } else if key_in_use(&app_key) {
            return true;
        }
    }
    false
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
fn webcam_in_use() -> bool {
    // A webcam is in use if any process has a /dev/video* device open.
    proc_has_open_path("/dev/video")
}

#[cfg(unix)]
fn mic_in_use() -> bool {
    // ALSA capture substreams report "RUNNING" in their status file while
    // recording. Approximate on PipeWire, which may keep the device open.
    use std::fs;
    let Ok(cards) = fs::read_dir("/proc/asound") else {
        return false;
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
                    return true;
                }
            }
        }
    }
    false
}

/// True if any process has an open file descriptor whose target starts with
/// `prefix` (e.g. "/dev/video").
#[cfg(unix)]
fn proc_has_open_path(prefix: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path())
                && target.to_string_lossy().starts_with(prefix)
            {
                return true;
            }
        }
    }
    false
}
