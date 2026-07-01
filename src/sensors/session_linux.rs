//! Session lock/unlock sensor (Linux).
//!
//! Polls logind's `LockedHint` for the current session and publishes
//! "locked"/"unlocked" to the `session` sensor. Mirrors the Windows WTS-based
//! `SessionSensor`; Linux has no cheap event, so it polls.

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct SessionSensor {
    state: Arc<AppState>,
}

impl SessionSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
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
                    let value = tokio::task::spawn_blocking(read_locked)
                        .await
                        .ok()
                        .flatten()
                        .map(|locked| if locked { "locked" } else { "unlocked" })
                        .unwrap_or("unknown");
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
fn read_locked() -> Option<bool> {
    use std::process::Command;
    let out = Command::new("loginctl")
        .args(["show-session", "self", "-p", "LockedHint", "--value"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
    Some(v == "yes" || v == "true")
}
