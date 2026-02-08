//! Idle time sensor for Linux - tracks last user input

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::process::Command;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, warn};

use crate::AppState;

pub struct IdleSensor {
    state: Arc<AppState>,
}

impl IdleSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        let interval_secs = config.intervals.last_active.max(1); // Prevent panic on 0
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Publish initial state
        let last_active = self.get_last_active_time();
        self.state
            .mqtt
            .publish_sensor("lastactive", &last_active.to_rfc3339())
            .await;

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Idle sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let last_active = self.get_last_active_time();
                    self.state.mqtt.publish_sensor("lastactive", &last_active.to_rfc3339()).await;
                }
            }
        }
    }

    fn get_last_active_time(&self) -> DateTime<Utc> {
        // Try xprintidle first (X11)
        if let Ok(output) = Command::new("xprintidle").output() {
            if output.status.success() {
                if let Ok(idle_str) = String::from_utf8(output.stdout) {
                    if let Ok(idle_ms) = idle_str.trim().parse::<i64>() {
                        return Utc::now() - ChronoDuration::milliseconds(idle_ms);
                    }
                }
            }
        }

        // Try qdbus for KDE/Wayland
        if let Ok(output) = Command::new("qdbus")
            .args([
                "org.freedesktop.ScreenSaver",
                "/ScreenSaver",
                "GetSessionIdleTime",
            ])
            .output()
        {
            if output.status.success() {
                if let Ok(idle_str) = String::from_utf8(output.stdout) {
                    if let Ok(idle_secs) = idle_str.trim().parse::<i64>() {
                        return Utc::now() - ChronoDuration::seconds(idle_secs);
                    }
                }
            }
        }

        // Fallback: just return now (no idle tracking available)
        warn!("No idle time detection available (install xprintidle for X11)");
        Utc::now()
    }
}
