//! Idle time sensor - tracks last user input and screensaver state

use std::sync::Arc;
use chrono::{DateTime, Utc, Duration as ChronoDuration};
use tokio::time::{interval, Duration};
use tracing::debug;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETSCREENSAVERRUNNING, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};

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
        self.state.mqtt.publish_sensor("lastactive", &last_active.to_rfc3339()).await;
        
        // Publish initial screensaver state (retained so HA picks it up)
        let screensaver_active = self.is_screensaver_running();
        self.state.mqtt.publish_sensor_retained("screensaver", if screensaver_active { "on" } else { "off" }).await;
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
                    
                    // Check screensaver state - only publish on change
                    let screensaver_active = self.is_screensaver_running();
                    if screensaver_active != prev_screensaver_state {
                        self.state.mqtt.publish_sensor_retained("screensaver", if screensaver_active { "on" } else { "off" }).await;
                        prev_screensaver_state = screensaver_active;
                        debug!("Screensaver state changed to: {}", if screensaver_active { "on" } else { "off" });
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
        unsafe {
            let mut running: i32 = 0;
            let result = SystemParametersInfoW(
                SPI_GETSCREENSAVERRUNNING,
                0,
                Some(&mut running as *mut i32 as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            result.is_ok() && running != 0
        }
    }
}
