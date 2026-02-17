//! Memory usage sensor - reports process memory consumption
#![allow(dead_code)] // Used on Windows only

use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::AppState;

pub struct MemorySensor {
    state: Arc<AppState>,
}

impl MemorySensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        // Report memory every 30 seconds
        let mut tick = interval(Duration::from_secs(30));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Publish initial state
        let memory_mb = get_memory_usage_mb();
        self.state
            .mqtt
            .publish_sensor("agent_memory", &format!("{:.1}", memory_mb))
            .await;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Memory sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let memory_mb = get_memory_usage_mb();
                    self.state.mqtt.publish_sensor("agent_memory", &format!("{:.1}", memory_mb)).await;
                }
            }
        }
    }
}

/// Get current process memory usage in MB (Private Working Set - matches Task Manager)
#[cfg(windows)]
fn get_memory_usage_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let process = GetCurrentProcess();
        // Use PROCESS_MEMORY_COUNTERS_EX to get PrivateUsage (matches Task Manager)
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };

        // Cast to PROCESS_MEMORY_COUNTERS* as GetProcessMemoryInfo expects that type
        let counters_ptr = (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>();

        if GetProcessMemoryInfo(process, counters_ptr, counters.cb).is_ok() {
            // PrivateUsage matches Task Manager's "Memory" column
            counters.PrivateUsage as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }
}

#[cfg(unix)]
fn get_memory_usage_mb() -> f64 {
    // Read from /proc/self/statm
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        let parts: Vec<&str> = statm.split_whitespace().collect();
        if let Some(rss_pages) = parts.get(1) {
            if let Ok(pages) = rss_pages.parse::<u64>() {
                // Pages are typically 4KB
                let page_size = 4096u64;
                return (pages * page_size) as f64 / (1024.0 * 1024.0);
            }
        }
    }
    0.0
}
