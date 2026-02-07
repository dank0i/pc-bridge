//! Idle time sensor - tracks last user input and screensaver state

use std::sync::Arc;
use std::os::windows::process::CommandExt;
use chrono::{DateTime, Utc, Duration as ChronoDuration};
use tokio::time::{interval, Duration};
use tracing::debug;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::System::SystemInformation::GetTickCount64;

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
        let screensaver_interval_secs = config.intervals.screensaver.max(1);
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut screensaver_tick = interval(Duration::from_secs(screensaver_interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Publish initial state
        let last_active = self.get_last_active_time();
        self.state.mqtt.publish_sensor("lastactive", &last_active.to_rfc3339()).await;
        
        // Publish initial screensaver state (retained so HA picks it up)
        let screensaver_active = self.is_screensaver_running();
        let screensaver_state = if screensaver_active { "on" } else { "off" };
        debug!("Initial screensaver state: {}", screensaver_state);
        self.state.mqtt.publish_sensor_retained("screensaver", screensaver_state).await;
        let mut prev_screensaver_state = screensaver_active;

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
                _ = screensaver_tick.tick() => {
                    // Check screensaver state - only publish on change
                    let screensaver_active = self.is_screensaver_running();
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

    fn get_last_active_time(&self) -> DateTime<Utc> {
        unsafe {
            let mut lii = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };

            if GetLastInputInfo(&mut lii).as_bool() {
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
    
    fn is_screensaver_running(&self) -> bool {
        // Check if any .scr process is running (same method we use to close them)
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-Process | Where-Object { $_.Path -like '*.scr' }).Count"
            ])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output();
        
        match output {
            Ok(out) => {
                let count_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let count: i32 = count_str.parse().unwrap_or(0);
                count > 0
            }
            Err(_) => false
        }
    }
}
