//! Display wake functions for Linux
#![allow(dead_code)] // Used on Linux only

use log::info;
use std::process::Command;

/// Wake display: bundled X11 (DPMS on + XTEST) first, then external-tool
/// fallbacks for setups where x11rb can't reach the display.
pub fn wake_display() {
    info!("WakeDisplay: Initiating display wake sequence (Linux)");

    // Bundled X11 (pure Rust, no external tools).
    if crate::linux_x11::wake() {
        info!("WakeDisplay: woke via x11");
        return;
    }

    // Fallbacks (Wayland / no x11rb).
    let _ = Command::new("xdotool").args(["key", "shift"]).status();
    let _ = Command::new("xset").args(["dpms", "force", "on"]).status();
    let _ = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.gnome.ScreenSaver",
            "--type=method_call",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver.SetActive",
            "boolean:false",
        ])
        .status();

    info!("WakeDisplay: Wake sequence completed");
}

/// Turn the display off (bundled X11 DPMS, falling back to `xset`).
pub fn monitor_off() {
    info!("MonitorOff: turning display off (Linux)");
    if crate::linux_x11::set_dpms(false) {
        return;
    }
    let _ = Command::new("xset").args(["dpms", "force", "off"]).status();
}
