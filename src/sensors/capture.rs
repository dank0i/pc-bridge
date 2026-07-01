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
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct CaptureSensor {
    state: Arc<AppState>,
}

impl CaptureSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let (mic, webcam) = {
            let c = self.state.config.read().await;
            (c.features.mic, c.features.webcam)
        };

        let mut tick = interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
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
                    // Scans block (registry on Windows, /proc on Linux), so run
                    // them off the single-threaded async runtime.
                    let (m, w) = tokio::task::spawn_blocking(move || {
                        (mic.then(mic_in_use), webcam.then(webcam_in_use))
                    })
                    .await
                    .unwrap_or((None, None));

                    if let Some(on) = m
                        && prev_mic != Some(on)
                    {
                        self.state.mqtt.publish_sensor_retained("mic", bool_state(on)).await;
                        prev_mic = Some(on);
                    }
                    if let Some(on) = w
                        && prev_cam != Some(on)
                    {
                        self.state.mqtt.publish_sensor_retained("webcam", bool_state(on)).await;
                        prev_cam = Some(on);
                    }
                }
            }
        }
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
