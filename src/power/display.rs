//! Display wake functions - handles waking display after WoL

use log::{error, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, keybd_event,
};
use windows::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SendMessageW};

const WM_SYSCOMMAND: u32 = 0x0112;
const SC_MONITORPOWER: usize = 0xF170;
const MONITOR_ON: isize = -1;
const VK_F15: u16 = 0x7E;

static SLEEP_PREVENTION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Wake display using multiple methods (matches Go WakeDisplay behavior)
pub fn wake_display() {
    info!("WakeDisplay: Initiating display wake sequence");

    // Dismiss screensaver first (kill .scr processes)
    dismiss_screensaver();

    // Turn on monitor
    turn_on_monitor();

    // Send benign keypress
    send_benign_keypress();

    // Temporarily prevent sleep
    prevent_sleep_temporary(Duration::from_secs(30));

    info!("WakeDisplay: Wake sequence completed");
}

/// Dismiss screensaver by terminating .scr processes natively via Win32 API
fn dismiss_screensaver() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    info!("Attempting to dismiss screensaver");

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &raw mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile);
                let name = name.trim_end_matches('\0');

                // Check for .scr extension (case-insensitive)
                if name.len() >= 4
                    && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".scr")
                {
                    if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                        info!(
                            "Terminating screensaver: {} (PID {})",
                            name, entry.th32ProcessID
                        );
                        let _ = TerminateProcess(handle, 0);
                        let _ = CloseHandle(handle);
                    }
                }

                if Process32NextW(snapshot, &raw mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    info!("Screensaver dismiss completed");
}

/// Wake display with retries (useful immediately after WoL)
pub fn wake_display_with_retry(max_attempts: usize, delay_between: Duration) {
    let attempts = max_attempts.max(1);
    info!(
        "WakeDisplay: Starting wake sequence with {} attempts",
        attempts
    );

    for attempt in 1..=attempts {
        dismiss_screensaver();
        std::thread::sleep(Duration::from_millis(50));
        turn_on_monitor();
        std::thread::sleep(Duration::from_millis(100));
        send_benign_keypress();

        if attempt < attempts {
            std::thread::sleep(delay_between);
        }
    }

    prevent_sleep_temporary(Duration::from_secs(30));
    info!("WakeDisplay: Wake sequence completed");
}

/// Send SC_MONITORPOWER to turn on all monitors
fn turn_on_monitor() {
    unsafe {
        SendMessageW(
            HWND_BROADCAST,
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER),
            LPARAM(MONITOR_ON),
        );
    }
}

/// Send F15 keypress to register user activity
/// F15 is rarely used by applications, won't trigger actions
fn send_benign_keypress() {
    unsafe {
        // Key down F15
        keybd_event(VK_F15 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(Duration::from_millis(10));
        // Key up F15
        keybd_event(VK_F15 as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

/// Temporarily prevent system sleep using SetThreadExecutionState
fn prevent_sleep_temporary(duration: Duration) {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
        SetThreadExecutionState,
    };

    // Only spawn one prevention goroutine at a time
    if SLEEP_PREVENTION_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    match std::thread::Builder::new()
        .name("sleep-prevent".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            unsafe {
                // Set execution state to prevent sleep
                let state = ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED;
                let ret = SetThreadExecutionState(state);

                if ret == EXECUTION_STATE::default() {
                    SLEEP_PREVENTION_ACTIVE.store(false, Ordering::SeqCst);
                    return;
                }

                std::thread::sleep(duration);

                // Reset to allow sleep again
                SetThreadExecutionState(ES_CONTINUOUS);
                SLEEP_PREVENTION_ACTIVE.store(false, Ordering::SeqCst);
                info!("WakeDisplay: Sleep prevention ended");
            }
        }) {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to spawn sleep prevention thread: {}", e);
            SLEEP_PREVENTION_ACTIVE.store(false, Ordering::SeqCst);
        }
    }
}
