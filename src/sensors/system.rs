//! System sensors - CPU, memory, battery, active window
//!
//! All implementations use native Win32 APIs for maximum performance.
//! No WMI, no PowerShell, no external processes.

use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::AppState;

/// System sensor that reports CPU, memory, battery, and active window
pub struct SystemSensor {
    state: Arc<AppState>,
}

/// Tracks previous sensor values to skip duplicate MQTT publishes
struct PrevSystemValues {
    cpu: String,
    mem: String,
    battery_level: String,
    battery_charging: String,
    active_window: String,
}

impl PrevSystemValues {
    fn new() -> Self {
        Self {
            cpu: String::new(),
            mem: String::new(),
            battery_level: String::new(),
            battery_charging: String::new(),
            active_window: String::new(),
        }
    }
}

impl SystemSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(10));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // CPU calculation needs previous sample
        let mut prev_cpu = get_cpu_times();
        let mut prev_vals = PrevSystemValues::new();

        // Initial publish (force all by using empty prev_vals)
        self.publish_all(&mut prev_cpu, &mut prev_vals).await;

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("System sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    self.publish_all(&mut prev_cpu, &mut prev_vals).await;
                }
            }
        }
    }

    async fn publish_all(&self, prev_cpu: &mut CpuTimes, prev: &mut PrevSystemValues) {
        // CPU usage (percentage)
        let cpu = calculate_cpu_usage(prev_cpu);
        let cpu_str = format!("{cpu:.1}");
        if cpu_str != prev.cpu {
            self.state.mqtt.publish_sensor("cpu_usage", &cpu_str).await;
            prev.cpu = cpu_str;
        }

        // Memory usage (percentage)
        let mem = get_memory_percent();
        let mem_str = format!("{mem:.1}");
        if mem_str != prev.mem {
            self.state
                .mqtt
                .publish_sensor("memory_usage", &mem_str)
                .await;
            prev.mem = mem_str;
        }

        // Battery (percentage and charging status)
        if let Some((percent, charging)) = get_battery_status() {
            let level_str = percent.to_string();
            let charging_str = if charging { "true" } else { "false" };
            if level_str != prev.battery_level {
                self.state
                    .mqtt
                    .publish_sensor("battery_level", &level_str)
                    .await;
                prev.battery_level = level_str;
            }
            if charging_str != prev.battery_charging {
                self.state
                    .mqtt
                    .publish_sensor("battery_charging", charging_str)
                    .await;
                prev.battery_charging = charging_str.to_string();
            }
        }

        // Active window title
        let title = get_active_window_title();
        if title != prev.active_window {
            self.state
                .mqtt
                .publish_sensor("active_window", &title)
                .await;
            prev.active_window = title;
        }
    }
}

// ============================================================================
// CPU Usage - Native via GetSystemTimes
// ============================================================================

#[derive(Default, Clone)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

#[cfg(windows)]
fn get_cpu_times() -> CpuTimes {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;

    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();

        if GetSystemTimes(
            Some(&raw mut idle),
            Some(&raw mut kernel),
            Some(&raw mut user),
        )
        .is_ok()
        {
            CpuTimes {
                idle: filetime_to_u64(idle),
                kernel: filetime_to_u64(kernel),
                user: filetime_to_u64(user),
            }
        } else {
            CpuTimes::default()
        }
    }
}

#[cfg(windows)]
fn filetime_to_u64(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

#[cfg(unix)]
fn get_cpu_times() -> CpuTimes {
    // Read /proc/stat
    if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
        if let Some(line) = stat.lines().next() {
            let parts: Vec<u64> = line
                .split_whitespace()
                .skip(1) // Skip "cpu"
                .filter_map(|s| s.parse().ok())
                .collect();

            if parts.len() >= 4 {
                // user, nice, system, idle
                let user = parts[0] + parts[1]; // user + nice
                let kernel = parts[2]; // system
                let idle = parts[3];
                return CpuTimes { idle, kernel, user };
            }
        }
    }
    CpuTimes::default()
}

fn calculate_cpu_usage(prev: &mut CpuTimes) -> f64 {
    let curr = get_cpu_times();

    let idle_delta = curr.idle.saturating_sub(prev.idle);
    let kernel_delta = curr.kernel.saturating_sub(prev.kernel);
    let user_delta = curr.user.saturating_sub(prev.user);

    // Total = kernel + user (kernel includes idle on Windows)
    #[cfg(windows)]
    let total = kernel_delta + user_delta;
    #[cfg(unix)]
    let total = idle_delta + kernel_delta + user_delta;

    let usage = if total > 0 {
        #[cfg(windows)]
        let busy = total - idle_delta;
        #[cfg(unix)]
        let busy = kernel_delta + user_delta;

        (busy as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    *prev = curr;
    usage.clamp(0.0, 100.0)
}

// ============================================================================
// Memory Usage - Native via GlobalMemoryStatusEx
// ============================================================================

#[cfg(windows)]
fn get_memory_percent() -> f64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    unsafe {
        let mut mem = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };

        if GlobalMemoryStatusEx(&raw mut mem).is_ok() {
            mem.dwMemoryLoad as f64
        } else {
            0.0
        }
    }
}

#[cfg(unix)]
fn get_memory_percent() -> f64 {
    // Read /proc/meminfo
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        let mut total: u64 = 0;
        let mut available: u64 = 0;

        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total = parse_meminfo_value(line);
            } else if line.starts_with("MemAvailable:") {
                available = parse_meminfo_value(line);
            }
        }

        if total > 0 {
            return ((total - available) as f64 / total as f64) * 100.0;
        }
    }
    0.0
}

#[cfg(unix)]
fn parse_meminfo_value(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

// ============================================================================
// Battery Status - Native via GetSystemPowerStatus
// ============================================================================

#[cfg(windows)]
fn get_battery_status() -> Option<(u8, bool)> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&raw mut status).is_ok() {
            // BatteryFlag 128 = no battery
            if status.BatteryFlag == 128 {
                return None;
            }

            let percent = if status.BatteryLifePercent == 255 {
                100 // Unknown = assume full
            } else {
                status.BatteryLifePercent
            };

            // ACLineStatus: 1 = plugged in
            let charging = status.ACLineStatus == 1;

            Some((percent, charging))
        } else {
            None
        }
    }
}

#[cfg(unix)]
fn get_battery_status() -> Option<(u8, bool)> {
    // Try common battery names: BAT0, BAT1, CMB0, etc.
    let power_supply = std::path::Path::new("/sys/class/power_supply");

    if let Ok(entries) = std::fs::read_dir(power_supply) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Look for battery devices (BAT*, CMB*, etc.)
            if name_str.starts_with("BAT") || name_str.starts_with("CMB") {
                let path = entry.path();

                let capacity = std::fs::read_to_string(path.join("capacity"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u8>().ok());

                if let Some(cap) = capacity {
                    let status = std::fs::read_to_string(path.join("status"))
                        .ok()
                        .unwrap_or_default();

                    let charging = status.trim() == "Charging";
                    return Some((cap, charging));
                }
            }
        }
    }
    None
}

// ============================================================================
// Active Window Title - Native via GetForegroundWindow
// ============================================================================

#[cfg(windows)]
fn get_active_window_title() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }

        let mut buffer = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buffer);

        if len > 0 {
            String::from_utf16_lossy(&buffer[..len as usize])
        } else {
            String::new()
        }
    }
}

#[cfg(unix)]
fn get_active_window_title() -> String {
    // Try xdotool for X11
    if let Ok(output) = std::process::Command::new("xdotool")
        .args(["getactivewindow", "getwindowname"])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    String::new()
}
