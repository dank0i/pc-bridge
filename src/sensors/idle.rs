//! Idle time sensor - tracks last user input and screensaver state
//!
//! Screensaver detection is event-driven via ProcessWatcher push notifications,
//! providing instant (~1s) MQTT updates when a .scr process starts or stops.
//! Last-active time is polled via GetLastInputInfo.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, info};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

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
        let mut process_rx = self.state.process_watcher.subscribe();

        // Publish initial state
        let last_active = self.get_last_active_time();
        self.state
            .mqtt
            .publish_sensor("lastactive", &last_active.to_rfc3339())
            .await;

        // Publish initial screensaver state (retained so HA picks it up)
        let screensaver_active = self.state.process_watcher.has_screensaver_running().await;
        let screensaver_state = if screensaver_active { "on" } else { "off" };
        debug!("Initial screensaver state: {}", screensaver_state);
        self.state
            .mqtt
            .publish_sensor_retained("screensaver", screensaver_state)
            .await;
        let mut prev_screensaver_state = screensaver_active;
        let mut prev_idle_secs: i64 = 0;

        info!("Idle sensor started (screensaver: push-based, lastactive: polled)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Idle sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let last_active = self.get_last_active_time();
                    let secs = last_active.timestamp();
                    if secs != prev_idle_secs {
                        self.state.mqtt.publish_sensor("lastactive", &last_active.to_rfc3339()).await;
                        prev_idle_secs = secs;
                    }
                }
                result = process_rx.recv() => {
                    // Process list changed — check screensaver state immediately
                    match result {
                        Ok(_notification) => {
                            let screensaver_active = self.state.process_watcher.has_screensaver_running().await;
                            if screensaver_active != prev_screensaver_state {
                                let state_str = if screensaver_active { "on" } else { "off" };
                                debug!("Screensaver state changed: {}", state_str);
                                self.state.mqtt.publish_sensor_retained("screensaver", state_str).await;
                                prev_screensaver_state = screensaver_active;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!("Idle sensor lagged {} notifications, re-checking screensaver", n);
                            let screensaver_active = self.state.process_watcher.has_screensaver_running().await;
                            if screensaver_active != prev_screensaver_state {
                                let state_str = if screensaver_active { "on" } else { "off" };
                                debug!("Screensaver state changed (post-lag): {}", state_str);
                                self.state.mqtt.publish_sensor_retained("screensaver", state_str).await;
                                prev_screensaver_state = screensaver_active;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("Process watcher channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    fn get_last_active_time(&self) -> DateTime<Utc> {
        unsafe {
            let mut lii = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };

            if GetLastInputInfo(&raw mut lii).as_bool() {
                let current_tick = GetTickCount64();
                // dwTime is 32-bit, handle wrap correctly
                let current_tick_32 = current_tick as u32;
                let idle_ms = current_tick_32.wrapping_sub(lii.dwTime) as i64;

                Utc::now() - ChronoDuration::milliseconds(idle_ms)
            } else {
                Utc::now()
            }
        }
    }
}
