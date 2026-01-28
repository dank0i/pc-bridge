//! Display wake functions for Linux

use std::process::Command;
use tracing::{info, debug};

/// Wake display using xdotool or dbus
pub fn wake_display() {
    info!("WakeDisplay: Initiating display wake sequence (Linux)");

    // Try xdotool to simulate key press (works on X11)
    let _ = Command::new("xdotool")
        .args(["key", "shift"])
        .spawn();

    // Try xset to turn on display
    let _ = Command::new("xset")
        .args(["dpms", "force", "on"])
        .spawn();

    // Try dbus for GNOME/KDE
    let _ = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.gnome.ScreenSaver",
            "--type=method_call",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver.SetActive",
            "boolean:false"
        ])
        .spawn();

    info!("WakeDisplay: Wake sequence completed");
}

/// Wake display with retries
pub fn wake_display_with_retry(max_attempts: usize, delay_between: std::time::Duration) {
    let attempts = max_attempts.max(1);
    info!("WakeDisplay: Starting wake sequence with {} attempts", attempts);

    for attempt in 1..=attempts {
        wake_display();
        if attempt < attempts {
            std::thread::sleep(delay_between);
        }
    }

    debug!("WakeDisplay: Wake sequence completed");
}
