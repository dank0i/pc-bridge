//! Session lock/unlock sensor (Linux).
//!
//! Polls logind's `LockedHint` for the current session and publishes
//! "locked"/"unlocked" to the `session` sensor. Mirrors the Windows WTS-based
//! `SessionSensor`; Linux has no cheap event, so it polls.

use log::{debug, info};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

static LOGINCTL_WARNED: AtomicBool = AtomicBool::new(false);

pub struct SessionSensor {
    state: Arc<AppState>,
}

impl SessionSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        let mut tick = interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = shutdown.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev: &'static str = "";

        info!("Session sensor started (Linux logind, polled every 5s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Session sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev = "";
                }
                _ = tick.tick() => {
                    // Fall back to "unlocked" (not "unknown") so the value vocab
                    // matches the Windows producer, which only emits locked/unlocked.
                    let value = tokio::task::spawn_blocking(read_locked)
                        .await
                        .ok()
                        .flatten()
                        .map_or("unlocked", |locked| if locked { "locked" } else { "unlocked" });
                    if value != prev {
                        self.state.mqtt.publish_sensor_retained("session", value).await;
                        prev = value;
                    }
                }
            }
        }
    }
}

/// Read `LockedHint` for the caller's session via loginctl. None if unavailable.
/// Note: only lockers that integrate with logind (GNOME/KDE) set LockedHint;
/// bare X11 lockers (xscreensaver, i3lock) will read as unlocked.
fn read_locked() -> Option<bool> {
    use std::process::Command;
    let out = match Command::new("loginctl")
        .args(["show-session", "self", "-p", "LockedHint", "--value"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound
                && !LOGINCTL_WARNED.swap(true, Ordering::Relaxed)
            {
                log::warn!(
                    "loginctl not found; session state will report 'unlocked' (needs systemd-logind)"
                );
            }
            return None;
        }
    };
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
    Some(v == "yes" || v == "true")
}
