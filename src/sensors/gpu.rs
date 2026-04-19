//! GPU usage sensor
//!
//! - Windows: uses PDH (Performance Data Helper) counters for GPU engine utilization
//! - Linux: reads /sys/class/drm/card0/device/gpu_busy_percent (AMD) or nvidia-smi

#[cfg(windows)]
use log::warn;
use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, interval};

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
        let interval_secs = config.intervals.system_sensors.max(1);
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
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
                    let gpu_str = get_gpu_usage();
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
    // Use PDH to query GPU engine utilization
    // The counter path for GPU is: \GPU Engine(*engtype_3D)\Utilization Percentage
    // This is complex to set up, so we use a simpler WMI approach via PowerShell-less method
    // For now, query the D3D adapter memory usage as a proxy via Win32_VideoController
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

    // SAFETY: PdhState contains raw isize handles (PDH query/counter) which are
    // only accessed from the single-threaded tokio runtime. Mutex satisfies the
    // Sync bound required for statics.
    unsafe impl Send for PdhState {}

    static PDH_INIT: OnceLock<Mutex<Option<PdhState>>> = OnceLock::new();

    let cell = PDH_INIT.get_or_init(|| {
        let state = unsafe {
            let mut query: isize = 0;
            let status = PdhOpenQueryW(None, 0, &mut query);
            if status.0 != 0 {
                warn!("PdhOpenQueryW failed: 0x{:08x}", status.0);
                return Mutex::new(None);
            }

            let counter_path =
                windows::core::w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage");
            let mut counter: isize = 0;
            let status = PdhAddEnglishCounterW(query, counter_path, 0, &mut counter);
            if status.0 != 0 {
                warn!("PdhAddEnglishCounterW failed: 0x{:08x}", status.0);
                let _ = PdhCloseQuery(query);
                return Mutex::new(None);
            }

            // Collect first sample (needed for rate counters)
            let _ = PdhCollectQueryData(query);

            Mutex::new(Some(PdhState {
                query,
                counter,
                has_first_sample: true,
            }))
        };
        state
    });

    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pdh) = guard.as_mut() else {
        return "0.0".to_string();
    };

    unsafe {
        if !pdh.has_first_sample {
            let _ = PdhCollectQueryData(pdh.query);
            pdh.has_first_sample = true;
            return "0.0".to_string();
        }

        let status = PdhCollectQueryData(pdh.query);
        if status.0 != 0 {
            return "0.0".to_string();
        }

        let mut value = PDH_FMT_COUNTERVALUE::default();
        let status = PdhGetFormattedCounterValue(pdh.counter, PDH_FMT_DOUBLE, None, &mut value);

        if status.0 == 0 && value.CStatus == PDH_CSTATUS_VALID_DATA.0 as u32 {
            format!("{:.1}", value.Anonymous.doubleValue.clamp(0.0, 100.0))
        } else {
            "0.0".to_string()
        }
    }
}

#[cfg(unix)]
fn get_gpu_usage() -> String {
    // Try AMD first: /sys/class/drm/card0/device/gpu_busy_percent
    if let Ok(val) = std::fs::read_to_string("/sys/class/drm/card0/device/gpu_busy_percent")
        && let Ok(pct) = val.trim().parse::<f64>()
    {
        return format!("{pct:.1}");
    }

    // Try NVIDIA via nvidia-smi
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        && output.status.success()
        && let Ok(pct) = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<f64>()
    {
        return format!("{pct:.1}");
    }

    "0.0".to_string()
}
