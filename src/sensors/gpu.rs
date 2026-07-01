//! GPU usage sensor
//!
//! - Windows: uses PDH (Performance Data Helper) counters for GPU engine utilization
//! - Linux: reads /sys/class/drm/card0/device/gpu_busy_percent (AMD) or nvidia-smi

#[cfg(windows)]
use log::warn;
use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct GpuSensor {
    state: Arc<AppState>,
}

impl GpuSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        if !config.features.gpu_sensor {
            return;
        }
        let interval_secs = config.intervals.gpu.max(1);
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));

        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev_gpu = String::new();

        info!("GPU sensor started (polled every {}s)", interval_secs);

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("GPU sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    // Force republish on reconnect
                    prev_gpu.clear();
                }
                _ = tick.tick() => {
                    // get_gpu_usage blocks (PDH collection on Windows, nvidia-smi
                    // fork+exec on Linux); keep it off the single-threaded runtime.
                    let Ok(gpu_str) = tokio::task::spawn_blocking(get_gpu_usage).await else {
                        continue;
                    };
                    if gpu_str != prev_gpu {
                        self.state.mqtt.publish_sensor("gpu_usage", &gpu_str).await;
                        prev_gpu = gpu_str;
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn get_gpu_usage() -> String {
    // Query GPU 3D-engine utilization via PDH performance counters:
    //   \GPU Engine(*engtype_3D)\Utilization Percentage
    // The query handle is persisted across calls (PDH needs two samples to
    // compute a rate).
    use windows::Win32::System::Performance::{
        PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PdhAddEnglishCounterW,
        PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW,
    };

    // PDH requires two samples to compute a rate, so we use a thread-local static
    // to persist the query handle across calls.
    use std::sync::{Mutex, OnceLock};

    struct PdhState {
        query: isize,
        counter: isize,
        has_first_sample: bool,
    }

    // SAFETY: PdhState contains raw isize handles (PDH query/counter). Access is
    // serialized by the enclosing `Mutex` and the sensor makes only one call per
    // tick, so the handles are never touched concurrently even though
    // spawn_blocking may run get_gpu_usage on different pool threads. PDH query
    // handles are not apartment-bound, so cross-thread (serialized) use is fine.
    unsafe impl Send for PdhState {}

    static PDH_INIT: OnceLock<Mutex<Option<PdhState>>> = OnceLock::new();

    let cell = PDH_INIT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());

    // Lazily (re)initialize. A boot-race where the "GPU Engine" perf-counter
    // provider isn't ready yet used to be cached as dead forever; retry on later
    // ticks instead (bounded by the poll interval).
    if guard.is_none() {
        *guard = unsafe {
            let mut query: isize = 0;
            let status = PdhOpenQueryW(None, 0, &raw mut query);
            if status != 0 {
                warn!("PdhOpenQueryW failed: 0x{:08x}", status);
                None
            } else {
                let counter_path =
                    windows::core::w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage");
                let mut counter: isize = 0;
                let status = PdhAddEnglishCounterW(query, counter_path, 0, &raw mut counter);
                if status != 0 {
                    warn!("PdhAddEnglishCounterW failed: 0x{:08x}", status);
                    let _ = PdhCloseQuery(query);
                    None
                } else {
                    // Collect first sample (needed for rate counters).
                    let _ = PdhCollectQueryData(query);
                    Some(PdhState {
                        query,
                        counter,
                        has_first_sample: true,
                    })
                }
            }
        };
    }

    let Some(pdh) = guard.as_mut() else {
        return "unavailable".to_string();
    };

    unsafe {
        if !pdh.has_first_sample {
            let _ = PdhCollectQueryData(pdh.query);
            pdh.has_first_sample = true;
            return "unavailable".to_string();
        }

        let status = PdhCollectQueryData(pdh.query);
        if status != 0 {
            return "unavailable".to_string();
        }

        let mut value = PDH_FMT_COUNTERVALUE::default();
        let status = PdhGetFormattedCounterValue(pdh.counter, PDH_FMT_DOUBLE, None, &raw mut value);

        if status == 0 && value.CStatus == PDH_CSTATUS_VALID_DATA {
            format!("{:.1}", value.Anonymous.doubleValue.clamp(0.0, 100.0))
        } else {
            "unavailable".to_string()
        }
    }
}

#[cfg(unix)]
fn get_gpu_usage() -> String {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Once we learn nvidia-smi isn't installed, stop forking it every tick.
    static NVIDIA_ABSENT: AtomicBool = AtomicBool::new(false);

    // Try AMD first: /sys/class/drm/card0/device/gpu_busy_percent
    if let Ok(val) = std::fs::read_to_string("/sys/class/drm/card0/device/gpu_busy_percent")
        && let Some(result) = parse_gpu_sysfs(&val)
    {
        return result;
    }

    // Try NVIDIA via nvidia-smi (unless we've already found it's absent).
    if !NVIDIA_ABSENT.load(Ordering::Relaxed) {
        match std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=utilization.gpu",
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(result) = parse_nvidia_smi_output(&stdout) {
                    return result;
                }
            }
            // Present but failed this tick (e.g. driver busy): keep trying.
            Ok(_) => {}
            // Not installed: don't fork it again. A transient spawn failure
            // (EAGAIN/ENOMEM under load) must NOT latch, or a working GPU would
            // read "unavailable" forever - so only give up on NotFound.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                NVIDIA_ABSENT.store(true, Ordering::Relaxed);
            }
            Err(_) => {}
        }
    }

    "unavailable".to_string()
}

/// Parse the AMD sysfs `gpu_busy_percent` file content.
#[cfg(unix)]
fn parse_gpu_sysfs(content: &str) -> Option<String> {
    let pct = content.trim().parse::<f64>().ok()?;
    Some(format!("{pct:.1}"))
}

/// Parse `nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits` output.
#[cfg(unix)]
fn parse_nvidia_smi_output(output: &str) -> Option<String> {
    let pct = output.trim().parse::<f64>().ok()?;
    Some(format!("{pct:.1}"))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_parse_gpu_sysfs_integer() {
        assert_eq!(parse_gpu_sysfs("42\n"), Some("42.0".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_gpu_sysfs_zero() {
        assert_eq!(parse_gpu_sysfs("0\n"), Some("0.0".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_gpu_sysfs_hundred() {
        assert_eq!(parse_gpu_sysfs("100"), Some("100.0".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_gpu_sysfs_garbage() {
        assert_eq!(parse_gpu_sysfs("N/A\n"), None);
        assert_eq!(parse_gpu_sysfs(""), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_nvidia_smi_typical() {
        assert_eq!(parse_nvidia_smi_output("73\n"), Some("73.0".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_nvidia_smi_with_spaces() {
        assert_eq!(
            parse_nvidia_smi_output("  55  \n"),
            Some("55.0".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_nvidia_smi_empty() {
        assert_eq!(parse_nvidia_smi_output(""), None);
    }
}
