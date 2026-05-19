//! Disk usage sensor
//!
//! Reports free/total/percent for configured paths.
//! - Windows: GetDiskFreeSpaceExW
//! - Linux: statvfs

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, interval};

use crate::AppState;

pub struct DiskSensor {
    state: Arc<AppState>,
}

impl DiskSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        if !config.features.disk_sensor {
            return;
        }
        let paths = config.disk_sensor_paths.clone();
        let interval_secs = config.intervals.system_sensors.max(1);
        drop(config);

        if paths.is_empty() {
            info!("Disk sensor enabled but no paths configured — skipping");
            return;
        }

        // Use a longer interval for disk (changes slowly)
        let poll_secs = (interval_secs * 6).max(60);
        let mut tick = interval(Duration::from_secs(poll_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev_state = String::new();

        info!(
            "Disk sensor started for {} path(s), polled every {}s",
            paths.len(),
            poll_secs
        );

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Disk sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev_state.clear();
                }
                _ = tick.tick() => {
                    let mut entries = Vec::new();
                    for path in &paths {
                        if let Some(info) = get_disk_usage(path) {
                            entries.push(serde_json::json!({
                                "path": path,
                                "total_gb": format!("{:.1}", info.total_bytes as f64 / 1_073_741_824.0),
                                "free_gb": format!("{:.1}", info.free_bytes as f64 / 1_073_741_824.0),
                                "used_percent": format!("{:.1}", info.used_percent),
                            }));
                        }
                    }

                    // State is the highest used_percent across all paths
                    let max_used: f64 = entries
                        .iter()
                        .filter_map(|e| e["used_percent"].as_str()?.parse::<f64>().ok())
                        .fold(0.0_f64, f64::max);
                    let state = format!("{max_used:.1}");

                    if state != prev_state {
                        self.state.mqtt.publish_sensor("disk_usage", &state).await;
                        let attrs = serde_json::json!({ "disks": entries });
                        self.state.mqtt.publish_sensor_attributes("disk_usage", &attrs).await;
                        prev_state = state;
                    }
                }
            }
        }
    }
}

struct DiskInfo {
    total_bytes: u64,
    free_bytes: u64,
    used_percent: f64,
}

impl DiskInfo {
    /// Compute `DiskInfo` from total and free byte counts.
    /// Returns `None` if `total` is zero.
    fn from_bytes(total: u64, free: u64) -> Option<Self> {
        if total == 0 {
            return None;
        }
        let used = total - free;
        Some(Self {
            total_bytes: total,
            free_bytes: free,
            used_percent: (used as f64 / total as f64) * 100.0,
        })
    }
}

#[cfg(windows)]
fn get_disk_usage(path: &str) -> Option<DiskInfo> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide_path: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut free_available: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;

        let ok = GetDiskFreeSpaceExW(
            windows::core::PCWSTR(wide_path.as_ptr()),
            Some(&mut free_available),
            Some(&mut total),
            Some(&mut total_free),
        );

        if ok.is_ok() {
            DiskInfo::from_bytes(total, total_free)
        } else {
            None
        }
    }
}

#[cfg(unix)]
fn get_disk_usage(path: &str) -> Option<DiskInfo> {
    use std::ffi::CString;

    let c_path = CString::new(path).ok()?;

    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &raw mut stat) != 0 {
            return None;
        }

        let block_size = stat.f_frsize as u64;
        let total = stat.f_blocks as u64 * block_size;
        let free = stat.f_bfree as u64 * block_size;

        DiskInfo::from_bytes(total, free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_info_from_bytes_typical() {
        // 1 TB total, 500 GB free → 50% used
        let total = 1_000_000_000_000;
        let free = 500_000_000_000;
        let info = DiskInfo::from_bytes(total, free).unwrap();
        assert_eq!(info.total_bytes, total);
        assert_eq!(info.free_bytes, free);
        assert!((info.used_percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_disk_info_from_bytes_full() {
        let info = DiskInfo::from_bytes(100, 0).unwrap();
        assert!((info.used_percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_disk_info_from_bytes_empty() {
        let info = DiskInfo::from_bytes(100, 100).unwrap();
        assert!((info.used_percent - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_disk_info_from_bytes_zero_total() {
        assert!(DiskInfo::from_bytes(0, 0).is_none());
    }

    #[test]
    fn test_disk_info_from_bytes_real_world() {
        // 512 GB SSD, 123 GB free → ~76% used
        let total: u64 = 512 * 1_073_741_824;
        let free: u64 = 123 * 1_073_741_824;
        let info = DiskInfo::from_bytes(total, free).unwrap();
        let expected = ((total - free) as f64 / total as f64) * 100.0;
        assert!((info.used_percent - expected).abs() < 0.01);
    }

    #[cfg(unix)]
    #[test]
    fn test_get_disk_usage_root() {
        // /tmp should always exist on unix
        let info = get_disk_usage("/tmp");
        assert!(info.is_some(), "/tmp should be accessible");
        let info = info.unwrap();
        assert!(info.total_bytes > 0);
        assert!(info.used_percent >= 0.0 && info.used_percent <= 100.0);
    }

    #[cfg(unix)]
    #[test]
    fn test_get_disk_usage_nonexistent() {
        let info = get_disk_usage("/nonexistent_path_that_should_never_exist");
        assert!(info.is_none());
    }
}
