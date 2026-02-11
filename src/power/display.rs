//! Display wake functions - handles waking display after WoL

use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::info;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
};
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, HWND_BROADCAST};

const WM_SYSCOMMAND: u32 = 0x0112;
const SC_MONITORPOWER: usize = 0xF170;
const MONITOR_ON: isize = -1;
const VK_F15: u16 = 0x7E;
const CREATE_NO_WINDOW: u32 = 0x08000000;

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

/// Dismiss screensaver by killing .scr processes via PowerShell
fn dismiss_screensaver() {
    info!("Attempting to dismiss screensaver");

    // Kill all screensaver processes (.scr) with PowerShell - matches Go version exactly
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Process | Where-Object { $_.Path -like '*.scr' } | Stop-Process -Force -ErrorAction SilentlyContinue"
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .and_then(|mut child| child.wait());

    info!("Screensaver dismiss attempted");
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
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
        EXECUTION_STATE,
    };

    // Only spawn one prevention goroutine at a time
    if SLEEP_PREVENTION_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    std::thread::spawn(move || {
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
    });
}
