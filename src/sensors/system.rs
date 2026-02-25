//! System sensors - CPU, memory, battery, active window
//!
//! - CPU/memory: polled (inherently sampled metrics)
//! - Battery: event-driven via RegisterPowerSettingNotification (instant on plug/unplug/level change)
//! - Active window: event-driven via SetWinEventHook(EVENT_SYSTEM_FOREGROUND) (instant on focus change)

#[cfg(windows)]
use log::error;
use log::{debug, info};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};

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
    health_uptime: u64,
}

impl PrevSystemValues {
    fn new() -> Self {
        Self {
            cpu: String::new(),
            mem: String::new(),
            battery_level: String::new(),
            battery_charging: String::new(),
            active_window: String::new(),
            health_uptime: 0,
        }
    }
}

/// Events from background threads monitoring window focus and battery state
#[cfg_attr(not(windows), allow(dead_code))]
enum SystemEvent {
    /// Foreground window changed
    WindowFocusChanged,
    /// Battery level or charging state changed
    BatteryChanged,
}

impl SystemSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        let interval_secs = config.intervals.system_sensors.max(1);
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // CPU calculation needs previous sample
        let mut prev_cpu = get_cpu_times();
        let mut prev_vals = PrevSystemValues::new();

        // Channel for receiving events from background threads
        let (event_tx, mut event_rx) = mpsc::channel::<SystemEvent>(8);

        // Start event-driven active window monitor
        #[cfg(windows)]
        {
            let tx = event_tx.clone();
            let shutdown = self.state.shutdown_tx.subscribe();
            start_window_focus_monitor(tx, shutdown);
        }

        // Start event-driven battery monitor
        #[cfg(windows)]
        {
            let tx = event_tx.clone();
            let shutdown = self.state.shutdown_tx.subscribe();
            start_battery_monitor(tx, shutdown);
        }

        // Suppress unused warning on non-Windows
        let _ = &event_tx;

        // Initial publish (force all by using empty prev_vals)
        self.publish_all(&mut prev_cpu, &mut prev_vals).await;

        // Track health publish separately (once per ~60s)
        let mut last_health_publish = tokio::time::Instant::now();

        info!("System sensor started (CPU/memory: polled, battery/active_window: event-driven)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("System sensor shutting down");
                    break;
                }
                // Hot-reload: pick up new interval from config changes
                Ok(()) = config_rx.recv() => {
                    let config = self.state.config.read().await;
                    let new_interval = config.intervals.system_sensors.max(1);
                    drop(config);
                    tick = interval(Duration::from_secs(new_interval));
                    debug!("System sensor: interval updated to {}s", new_interval);
                }
                _ = tick.tick() => {
                    // Polled: CPU and memory only
                    self.publish_cpu_mem(&mut prev_cpu, &mut prev_vals).await;

                    // Bridge health diagnostics (~every 60s)
                    if last_health_publish.elapsed() >= Duration::from_secs(60) {
                        last_health_publish = tokio::time::Instant::now();
                        self.publish_health(&mut prev_vals).await;
                    }
                }
                Some(event) = event_rx.recv() => {
                    match event {
                        SystemEvent::WindowFocusChanged => {
                            let title = get_active_window_title();
                            if title != prev_vals.active_window {
                                self.state.mqtt.publish_sensor("active_window", &title).await;
                                prev_vals.active_window = title;
                            }
                        }
                        SystemEvent::BatteryChanged => {
                            if let Some((percent, charging)) = get_battery_status() {
                                let level_str = percent.to_string();
                                let charging_str = if charging { "true" } else { "false" };
                                if level_str != prev_vals.battery_level {
                                    self.state.mqtt.publish_sensor("battery_level", &level_str).await;
                                    prev_vals.battery_level = level_str;
                                }
                                if charging_str != prev_vals.battery_charging {
                                    self.state.mqtt.publish_sensor("battery_charging", charging_str).await;
                                    prev_vals.battery_charging = charging_str.to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn publish_cpu_mem(&self, prev_cpu: &mut CpuTimes, prev: &mut PrevSystemValues) {
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
    }

    async fn publish_all(&self, prev_cpu: &mut CpuTimes, prev: &mut PrevSystemValues) {
        // CPU and memory (polled metrics)
        self.publish_cpu_mem(prev_cpu, prev).await;

        // Bridge health (initial publish)
        self.publish_health(prev).await;

        // Battery (event-driven, but publish initial state)
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

        // Active window title (event-driven, but publish initial state)
        #[cfg(windows)]
        let title = get_active_window_title();
        #[cfg(unix)]
        let title = get_active_window_title_async().await;
        if title != prev.active_window {
            self.state
                .mqtt
                .publish_sensor("active_window", &title)
                .await;
            prev.active_window = title;
        }
    }

    /// Publish bridge health diagnostics (uptime, version)
    async fn publish_health(&self, prev: &mut PrevSystemValues) {
        let uptime_secs = self.state.start_time.elapsed().as_secs();
        // Only publish when uptime actually changed (avoids duplicate on rapid calls)
        if uptime_secs != prev.health_uptime {
            self.state
                .mqtt
                .publish_sensor("bridge_health", &uptime_secs.to_string())
                .await;
            let attrs = serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
            });
            self.state
                .mqtt
                .publish_sensor_attributes("bridge_health", &attrs)
                .await;
            prev.health_uptime = uptime_secs;
        }
    }
}

// ============================================================================
// Active Window Monitor - Event-driven via SetWinEventHook
// ============================================================================

#[cfg(windows)]
fn start_window_focus_monitor(
    event_tx: mpsc::Sender<SystemEvent>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Accessibility::SetWinEventHook;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
        PostMessageW, RegisterClassExW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_USER,
        WNDCLASSEXW,
    };

    // Thread-local sender for the WinEvent callback
    thread_local! {
        static FOCUS_TX: std::cell::RefCell<Option<mpsc::Sender<SystemEvent>>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn win_event_proc(
        _h_win_event_hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
        _event: u32,
        _hwnd: HWND,
        _id_object: i32,
        _id_child: i32,
        _id_event_thread: u32,
        _dwms_event_time: u32,
    ) {
        FOCUS_TX.with(|tx| {
            if let Some(sender) = tx.borrow().as_ref() {
                let _ = sender.blocking_send(SystemEvent::WindowFocusChanged);
            }
        });
    }

    // Spawn a thread with a message pump for SetWinEventHook
    let (hwnd_tx, hwnd_rx) = tokio::sync::oneshot::channel::<isize>();

    match std::thread::Builder::new()
        .name("window-focus".into())
        .stack_size(256 * 1024)
        .spawn(move || unsafe {
            // Store sender in thread-local
            FOCUS_TX.with(|tx| {
                *tx.borrow_mut() = Some(event_tx);
            });

            let class_name = windows::core::w!("PCAgentFocusMonitor");
            // Wrapper needed because DefWindowProcW is generic and doesn't
            // match the extern "system" fn pointer expected by WNDCLASSEXW.
            unsafe extern "system" fn focus_wnd_proc(
                hwnd: HWND,
                msg: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> windows::Win32::Foundation::LRESULT {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(focus_wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassExW(&raw const wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Focus Monitor"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    error!("Failed to create focus monitor window: {:?}", e);
                    let _ = hwnd_tx.send(0);
                    return;
                }
            };

            // EVENT_SYSTEM_FOREGROUND = 0x0003
            let hook = SetWinEventHook(
                0x0003, // EVENT_SYSTEM_FOREGROUND
                0x0003, // EVENT_SYSTEM_FOREGROUND
                None,   // No DLL
                Some(win_event_proc),
                0,      // All processes
                0,      // All threads
                0x0002, // WINEVENT_OUTOFCONTEXT
            );

            if hook.is_invalid() {
                error!("Failed to set WinEventHook for foreground window");
            } else {
                debug!("WinEventHook set for EVENT_SYSTEM_FOREGROUND");
            }

            let _ = hwnd_tx.send(hwnd.0 as isize);

            // Message pump
            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&raw mut msg, None, 0, 0);
                if !ret.as_bool() || ret.0 == -1 {
                    break;
                }
                if msg.message == WM_USER {
                    break;
                }
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }

            let _ = DestroyWindow(hwnd);
        }) {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to spawn window focus thread: {}", e);
            return;
        }
    }

    // Spawn task to post WM_QUIT on shutdown
    tokio::spawn(async move {
        let hwnd_val = hwnd_rx.await.unwrap_or(0);
        if hwnd_val != 0 {
            let _ = shutdown_rx.recv().await;
            unsafe {
                let hwnd = HWND(hwnd_val as *mut _);
                let _ = PostMessageW(hwnd, WM_USER, WPARAM(0), LPARAM(0));
            }
        }
    });
}

// ============================================================================
// Battery Monitor - Event-driven via RegisterPowerSettingNotification
// ============================================================================

#[cfg(windows)]
fn start_battery_monitor(
    event_tx: mpsc::Sender<SystemEvent>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Power::RegisterPowerSettingNotification;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, MSG, PostMessageW,
        RegisterClassExW, SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_USER, WNDCLASSEXW,
    };

    const WM_POWERBROADCAST: u32 = 0x218;
    const PBT_POWERSETTINGCHANGE: usize = 0x8013;

    /// GUID_BATTERY_PERCENTAGE_REMAINING: {A7AD8041-B45A-4CAE-87A3-EECBB468A9E1}
    const GUID_BATTERY_PERCENTAGE_REMAINING: windows::core::GUID = windows::core::GUID::from_values(
        0xA7AD_8041,
        0xB45A,
        0x4CAE,
        [0x87, 0xA3, 0xEE, 0xCB, 0xB4, 0x68, 0xA9, 0xE1],
    );

    /// GUID_ACDC_POWER_SOURCE: {5D3E9A59-E9D5-4B00-A6BD-FF34FF516548}
    const GUID_ACDC_POWER_SOURCE: windows::core::GUID = windows::core::GUID::from_values(
        0x5D3E_9A59,
        0xE9D5,
        0x4B00,
        [0xA6, 0xBD, 0xFF, 0x34, 0xFF, 0x51, 0x65, 0x48],
    );

    #[repr(C)]
    struct PowerBroadcastSetting {
        power_setting: windows::core::GUID,
        data_length: u32,
        data: [u8; 1],
    }

    unsafe extern "system" fn battery_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if msg == WM_POWERBROADCAST && wparam.0 == PBT_POWERSETTINGCHANGE {
                let pbs = lparam.0 as *const PowerBroadcastSetting;
                if !pbs.is_null() {
                    let setting = &*pbs;
                    if (setting.power_setting == GUID_BATTERY_PERCENTAGE_REMAINING
                        || setting.power_setting == GUID_ACDC_POWER_SOURCE)
                        && setting.data_length >= 1
                    {
                        let event_tx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA)
                            as *const mpsc::Sender<SystemEvent>;
                        if !event_tx_ptr.is_null() {
                            let event_tx = &*event_tx_ptr;
                            let _ = event_tx.blocking_send(SystemEvent::BatteryChanged);
                        }
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }

    let (hwnd_tx, hwnd_rx) = tokio::sync::oneshot::channel::<isize>();

    match std::thread::Builder::new()
        .name("battery-monitor".into())
        .stack_size(256 * 1024)
        .spawn(move || unsafe {
            let class_name = windows::core::w!("PCAgentBatteryMonitor");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(battery_wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassExW(&raw const wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Battery Monitor"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    error!("Failed to create battery monitor window: {:?}", e);
                    let _ = hwnd_tx.send(0);
                    return;
                }
            };

            // Register for battery percentage changes
            if let Err(e) = RegisterPowerSettingNotification(
                HANDLE(hwnd.0),
                &GUID_BATTERY_PERCENTAGE_REMAINING,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            ) {
                error!("Failed to register battery level notification: {:?}", e);
            }

            // Register for AC/DC power source changes (plug/unplug)
            if let Err(e) = RegisterPowerSettingNotification(
                HANDLE(hwnd.0),
                &GUID_ACDC_POWER_SOURCE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            ) {
                error!("Failed to register power source notification: {:?}", e);
            }

            // Store event_tx in window's user data
            let event_tx_box = Box::new(event_tx);
            let event_tx_ptr = Box::into_raw(event_tx_box);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, event_tx_ptr as isize);

            debug!("Battery monitor registered for power notifications");
            let _ = hwnd_tx.send(hwnd.0 as isize);

            // Message pump
            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&raw mut msg, None, 0, 0);
                if !ret.as_bool() || ret.0 == -1 {
                    break;
                }
                if msg.message == WM_USER {
                    break;
                }
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }

            // Cleanup
            let _ = Box::from_raw(event_tx_ptr);
            let _ = DestroyWindow(hwnd);
        }) {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to spawn battery monitor thread: {}", e);
            return;
        }
    }

    // Spawn task to post WM_USER on shutdown
    tokio::spawn(async move {
        let hwnd_val = hwnd_rx.await.unwrap_or(0);
        if hwnd_val != 0 {
            let _ = shutdown_rx.recv().await;
            unsafe {
                let hwnd = HWND(hwnd_val as *mut _);
                let _ = PostMessageW(hwnd, WM_USER, WPARAM(0), LPARAM(0));
            }
        }
    });
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
    if let Ok(stat) = std::fs::read_to_string("/proc/stat")
        && let Some(line) = stat.lines().next()
    {
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

/// Truncate window titles longer than 256 bytes to prevent oversized MQTT payloads.
/// Ensures truncation happens on a UTF-8 character boundary.
fn truncate_title(title: String) -> String {
    if title.len() <= 256 {
        return title;
    }
    // Find a safe truncation point that doesn't split a UTF-8 char
    let mut end = 253;
    while end > 0 && !title.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &title[..end]) // \u{2026} = '…'
}

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
            truncate_title(String::from_utf16_lossy(&buffer[..len as usize]))
        } else {
            String::new()
        }
    }
}

#[cfg(unix)]
fn get_active_window_title() -> String {
    // Blocking subprocess — must not run directly on the single-threaded async runtime.
    // Callers that need async should use spawn_blocking(get_active_window_title).
    get_active_window_title_blocking()
}

/// Async wrapper for use from tokio tasks on Linux
#[cfg(unix)]
pub async fn get_active_window_title_async() -> String {
    tokio::task::spawn_blocking(get_active_window_title_blocking)
        .await
        .unwrap_or_default()
}

#[cfg(unix)]
fn get_active_window_title_blocking() -> String {
    // Try xdotool for X11
    if let Ok(output) = std::process::Command::new("xdotool")
        .args(["getactivewindow", "getwindowname"])
        .output()
        && output.status.success()
    {
        return truncate_title(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    String::new()
}
