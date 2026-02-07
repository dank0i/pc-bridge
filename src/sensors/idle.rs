//! Idle time sensor - tracks last user input and screensaver state

use std::sync::Arc;
use chrono::{DateTime, Utc, Duration as ChronoDuration};
use tokio::time::{interval, Duration};
use tracing::debug;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::Foundation::CloseHandle;

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
        // Use native ToolHelp API to check for .scr processes (~2-5ms vs 200-500ms for PowerShell)
        unsafe {
            let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                Ok(s) => s,
                Err(_) => return false,
            };

            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };

            let mut found = false;
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let name = String::from_utf16_lossy(&entry.szExeFile)
                        .trim_end_matches('\0')
                        .to_lowercase();
                    
                    if name.ends_with(".scr") {
                        found = true;
                        break;
                    }

                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }

            let _ = CloseHandle(snapshot);
            found
        }
    }
}
