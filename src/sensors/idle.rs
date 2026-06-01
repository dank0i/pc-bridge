//! Idle time sensor - tracks last user input and screensaver state
//!
//! Screensaver detection is event-driven via ProcessWatcher push notifications,
//! providing instant (~1s) MQTT updates when a .scr process starts or stops.
//! Idle time is polled via GetLastInputInfo and published two ways:
//!   - `idle_seconds`: seconds since last keyboard/mouse input (numeric, HA-friendly)
//!   - `lastactive`:   RFC3339 timestamp of last input (frozen while idle)
//!
//! IMPORTANT: GetLastInputInfo only reports input for the session the calling
//! process is attached to. If the bridge ever runs outside the interactive user
//! session (e.g. as a session-0 service) the call fails; we surface that as a
//! warning and pause updates rather than fabricating "active now".

use log::{debug, info, warn};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::{MissedTickBehavior, interval};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

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

        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut process_rx = self.state.process_watcher.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // Dedup trackers: only publish when the whole-second value changes.
        // `idle_seconds` grows each tick while idle (so it keeps publishing);
        // `lastactive` freezes while idle (so it stops publishing) - both correct.
        let mut prev_idle_secs: i64 = -1;
        let mut prev_lastactive_secs: i64 = i64::MIN;
        let mut query_failed = false;

        // Publish initial idle state
        self.publish_idle(
            &mut prev_idle_secs,
            &mut prev_lastactive_secs,
            &mut query_failed,
        )
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

        info!("Idle sensor started (screensaver: push-based, idle: polled via GetLastInputInfo)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Idle sensor shutting down");
                    break;
                }
                // Hot-reload: pick up new interval from config changes
                Ok(()) = config_rx.recv() => {
                    let config = self.state.config.read().await;
                    let new_interval = config.intervals.last_active.max(1);
                    drop(config);
                    tick = interval(Duration::from_secs(new_interval));
                    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    debug!("Idle sensor: interval updated to {}s", new_interval);
                }
                _ = tick.tick() => {
                    self.publish_idle(&mut prev_idle_secs, &mut prev_lastactive_secs, &mut query_failed).await;
                }
                result = process_rx.recv() => {
                    // Process list changed - check screensaver state immediately
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

    /// Poll idle time and publish `idle_seconds` + `lastactive`.
    ///
    /// On query failure we keep the last published value rather than fabricating
    /// "active now" - the old behaviour made the PC look perpetually busy, which
    /// broke any "PC idle for N minutes" automation downstream.
    async fn publish_idle(
        &self,
        prev_idle_secs: &mut i64,
        prev_lastactive_secs: &mut i64,
        query_failed: &mut bool,
    ) {
        let Some(idle_ms) = self.get_idle_ms() else {
            if !*query_failed {
                warn!(
                    "GetLastInputInfo failed - pausing idle updates (last values retained). \
                     The bridge must run in the interactive user session for idle tracking to work."
                );
                *query_failed = true;
            }
            return;
        };
        if *query_failed {
            info!("GetLastInputInfo recovered; resuming idle updates");
            *query_failed = false;
        }

        let idle_secs = (idle_ms / 1000).max(0);
        debug!("Idle: {idle_secs}s since last input");

        // idle_seconds - numeric, grows while idle, resets to ~0 on input.
        if idle_secs != *prev_idle_secs {
            self.state
                .mqtt
                .publish_sensor("idle_seconds", &idle_secs.to_string())
                .await;
            *prev_idle_secs = idle_secs;
        }

        // lastactive - timestamp of last input; stays frozen (no republish) while idle.
        let last_active = OffsetDateTime::now_utc() - time::Duration::milliseconds(idle_ms);
        let la_secs = last_active.unix_timestamp();
        if la_secs != *prev_lastactive_secs {
            self.state
                .mqtt
                .publish_sensor("lastactive", &format_rfc3339(last_active))
                .await;
            *prev_lastactive_secs = la_secs;
        }
    }

    /// Milliseconds since the last keyboard/mouse input, or `None` if the query
    /// failed (e.g. no access to the interactive input desktop).
    fn get_idle_ms(&self) -> Option<i64> {
        unsafe {
            let mut lii = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };

            if GetLastInputInfo(&raw mut lii).as_bool() {
                // dwTime is the low 32 bits of the millisecond tick count; match it.
                let current_tick_32 = GetTickCount64() as u32;
                Some(current_tick_32.wrapping_sub(lii.dwTime) as i64)
            } else {
                None
            }
        }
    }
}
