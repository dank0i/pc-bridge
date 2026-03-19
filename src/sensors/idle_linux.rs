//! Idle time sensor for Linux - tracks last user input and screensaver state
//!
//! Screensaver detection uses D-Bus org.freedesktop.ScreenSaver or xdg-screensaver.
//! Last-active time uses xprintidle (X11) or qdbus (KDE/Wayland).

use log::{debug, info, warn};
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
        let mut interval_secs = config.intervals.last_active.max(1); // Prevent panic on 0
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

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

        // Publish initial screensaver state (retained so HA picks it up)
        let screensaver_active = is_screensaver_active();
        let screensaver_state = if screensaver_active { "on" } else { "off" };
        debug!("Initial screensaver state: {}", screensaver_state);
        self.state
            .mqtt
            .publish_sensor_retained("screensaver", screensaver_state)
            .await;
        let mut prev_screensaver_state = screensaver_active;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Idle sensor shutting down");
                    break;
                }
                // Config hot-reload: update poll interval
                Ok(()) = config_rx.recv() => {
                    let config = self.state.config.read().await;
                    let new_interval = config.intervals.last_active.max(1);
                    if new_interval != interval_secs {
                        interval_secs = new_interval;
                        tick = interval(Duration::from_secs(interval_secs));
                        info!("Idle sensor: interval changed to {}s", interval_secs);
                    }
                }
                // MQTT reconnected: force republish current state
                Ok(()) = reconnect_rx.recv() => {
                    info!("Idle sensor: MQTT reconnected, republishing current state");
                    let last_active = self.get_last_active_time().await;
                    let formatted = format_rfc3339(last_active);
                    self.state.mqtt.publish_sensor("lastactive", &formatted).await;
                    prev_last_active = formatted;
                }
                _ = tick.tick() => {
                    let last_active = self.get_last_active_time().await;
                    let formatted = format_rfc3339(last_active);
                    if formatted != prev_last_active {
                        self.state.mqtt.publish_sensor("lastactive", &formatted).await;
                        prev_last_active = formatted;
                    }

                    // Check screensaver state (polled alongside idle time)
                    let screensaver_active = is_screensaver_active();
                    if screensaver_active != prev_screensaver_state {
                        let state_str = if screensaver_active { "on" } else { "off" };
                        debug!("Screensaver state changed: {}", state_str);
                        self.state.mqtt.publish_sensor_retained("screensaver", state_str).await;
                        prev_screensaver_state = screensaver_active;
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

/// Check if a screensaver is currently active on Linux.
///
/// Tries multiple detection methods:
/// 1. xdg-screensaver status (most portable)
/// 2. D-Bus org.freedesktop.ScreenSaver.GetActive (GNOME/KDE)
/// 3. D-Bus org.gnome.ScreenSaver.GetActive (GNOME-specific)
fn is_screensaver_active() -> bool {
    // Try xdg-screensaver status (returns "enabled" when active)
    if let Ok(output) = Command::new("xdg-screensaver").arg("status").output()
        && output.status.success()
        && let Ok(status) = String::from_utf8(output.stdout)
    {
        return status.trim() == "enabled";
    }

    // Try freedesktop D-Bus interface
    if let Ok(output) = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.ScreenSaver",
            "--type=method_call",
            "--print-reply",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver.GetActive",
        ])
        .output()
        && output.status.success()
        && let Ok(reply) = String::from_utf8(output.stdout)
    {
        return reply.contains("boolean true");
    }

    // Try GNOME-specific D-Bus interface
    if let Ok(output) = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.gnome.ScreenSaver",
            "--type=method_call",
            "--print-reply",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver.GetActive",
        ])
        .output()
        && output.status.success()
        && let Ok(reply) = String::from_utf8(output.stdout)
    {
        return reply.contains("boolean true");
    }

    false
}
