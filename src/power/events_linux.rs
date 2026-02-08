//! Power event listener for Linux - detects sleep/wake via systemd/dbus

use std::process::Command;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, info};

use crate::AppState;

pub struct PowerEventListener {
    state: Arc<AppState>,
}

impl PowerEventListener {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // On Linux, we poll systemd's sleep state or use dbus-monitor
        // This is a simplified polling approach
        let mut tick = interval(Duration::from_secs(5));
        let mut was_sleeping = false;

        info!("Power event listener started (Linux polling mode)");

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Power listener shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let is_sleeping = self.check_sleep_state();

                    if is_sleeping && !was_sleeping {
                        info!("Power event: SLEEP");
                        self.state.mqtt.publish_sensor_retained("sleep_state", "sleeping").await;
                    } else if !is_sleeping && was_sleeping {
                        info!("Power event: WAKE");
                        self.state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
                    }

                    was_sleeping = is_sleeping;
                }
            }
        }
    }

    fn check_sleep_state(&self) -> bool {
        // Check via systemctl if the system is preparing to sleep
        // This is a basic check - for real-time events, dbus-monitor would be better
        if let Ok(output) = Command::new("systemctl")
            .args(["is-active", "sleep.target"])
            .output()
        {
            if let Ok(state) = String::from_utf8(output.stdout) {
                return state.trim() == "active";
            }
        }
        false
    }
}
