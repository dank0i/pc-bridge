//! System uptime sensor
//!
//! Reports the system (OS) uptime in seconds.
//! - Windows: GetTickCount64
//! - Linux: /proc/uptime

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, interval};

use crate::AppState;

pub struct UptimeSensor {
    state: Arc<AppState>,
}

impl UptimeSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        if !config.features.uptime_sensor {
            return;
        }
        drop(config);

        // Uptime changes slowly — poll every 60 seconds
        let mut tick = interval(Duration::from_mins(1));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev_uptime = String::new();

        info!("Uptime sensor started (polled every 60s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Uptime sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev_uptime.clear();
                }
                _ = tick.tick() => {
                    let secs = get_system_uptime();
                    let uptime_str = secs.to_string();
                    if uptime_str != prev_uptime {
                        self.state.mqtt.publish_sensor("system_uptime", &uptime_str).await;
                        prev_uptime = uptime_str;
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn get_system_uptime() -> u64 {
    // GetTickCount64 returns milliseconds since system boot
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() / 1000 }
}

#[cfg(unix)]
fn get_system_uptime() -> u64 {
    if let Ok(content) = std::fs::read_to_string("/proc/uptime") {
        return parse_proc_uptime(&content);
    }
    0
}

/// Parse the first field of `/proc/uptime` as whole seconds.
#[cfg(unix)]
fn parse_proc_uptime(content: &str) -> u64 {
    if let Some(secs_str) = content.split_whitespace().next()
        && let Ok(secs) = secs_str.parse::<f64>()
    {
        return secs as u64;
    }
    0
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_uptime_typical() {
        // /proc/uptime: "uptime_secs idle_secs"
        assert_eq!(parse_proc_uptime("123456.78 98765.43"), 123_456);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_uptime_integer() {
        assert_eq!(parse_proc_uptime("3600 1800"), 3600);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_uptime_empty() {
        assert_eq!(parse_proc_uptime(""), 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_uptime_garbage() {
        assert_eq!(parse_proc_uptime("not_a_number foo"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_uptime_large() {
        // ~115 days
        assert_eq!(parse_proc_uptime("10000000.99 5000000.00"), 10_000_000);
    }
}
