//! Idle time sensor for Linux - tracks last user input

use log::{debug, warn};
use std::process::Command;
use std::sync::Arc;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::{Duration, interval};

use crate::AppState;

/// Format an OffsetDateTime as RFC 3339 string
fn format_rfc3339(dt: OffsetDateTime) -> String {
    dt.format(&Rfc3339).unwrap_or_else(|_| dt.to_string())
}

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

        // Track previous value to skip duplicate publishes
        let mut prev_last_active;

        // Publish initial state
        let last_active = self.get_last_active_time().await;
        let formatted = format_rfc3339(last_active);
        self.state
            .mqtt
            .publish_sensor("lastactive", &formatted)
            .await;
        prev_last_active = formatted;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Idle sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let last_active = self.get_last_active_time().await;
                    let formatted = format_rfc3339(last_active);
                    if formatted != prev_last_active {
                        self.state.mqtt.publish_sensor("lastactive", &formatted).await;
                        prev_last_active = formatted;
                    }
                }
            }
        }
    }

    async fn get_last_active_time(&self) -> OffsetDateTime {
        // Blocking subprocess calls — run off the single-threaded runtime
        tokio::task::spawn_blocking(Self::get_last_active_time_blocking)
            .await
            .unwrap_or_else(|_| OffsetDateTime::now_utc())
    }

    fn get_last_active_time_blocking() -> OffsetDateTime {
        // Try xprintidle first (X11)
        if let Ok(output) = Command::new("xprintidle").output()
            && output.status.success()
            && let Ok(idle_str) = String::from_utf8(output.stdout)
            && let Ok(idle_ms) = idle_str.trim().parse::<i64>()
        {
            return OffsetDateTime::now_utc() - time::Duration::milliseconds(idle_ms);
        }

        // Try qdbus for KDE/Wayland
        if let Ok(output) = Command::new("qdbus")
            .args([
                "org.freedesktop.ScreenSaver",
                "/ScreenSaver",
                "GetSessionIdleTime",
            ])
            .output()
            && output.status.success()
            && let Ok(idle_str) = String::from_utf8(output.stdout)
            && let Ok(idle_secs) = idle_str.trim().parse::<i64>()
        {
            return OffsetDateTime::now_utc() - time::Duration::seconds(idle_secs);
        }

        // Fallback: just return now (no idle tracking available)
        warn!("No idle time detection available (install xprintidle for X11)");
        OffsetDateTime::now_utc()
    }
}
