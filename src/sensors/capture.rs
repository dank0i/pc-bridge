//! Microphone / webcam in-use sensors (Windows).
//!
//! Polls the Windows CapabilityAccessManager consent store. Each app that has
//! used the mic/camera has a `LastUsedTimeStop` value; when it is 0 the app is
//! using the device right now. Publishes "on"/"off" to the `mic` and `webcam`
//! sensors when the aggregate in-use state changes.

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

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
                    if mic {
                        let on = capability_in_use("microphone");
                        if prev_mic != Some(on) {
                            self.state.mqtt.publish_sensor_retained("mic", bool_state(on)).await;
                            prev_mic = Some(on);
                        }
                    }
                    if webcam {
                        let on = capability_in_use("webcam");
                        if prev_cam != Some(on) {
                            self.state.mqtt.publish_sensor_retained("webcam", bool_state(on)).await;
                            prev_cam = Some(on);
                        }
                    }
                }
            }
        }
    }
}

fn bool_state(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// True if any app currently holds the given capability ("microphone"/"webcam").
fn capability_in_use(capability: &str) -> bool {
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
fn key_in_use(key: &RegKey) -> bool {
    key.get_value::<u64, _>("LastUsedTimeStop")
        .map(|v| v == 0)
        .unwrap_or(false)
}
