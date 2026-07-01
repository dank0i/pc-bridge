//! Display wake functions for Linux
#![allow(dead_code)] // Used on Linux only

use log::info;
use std::process::Command;

/// Wake display: bundled X11 (DPMS on + XTEST) first, then external-tool
/// fallbacks for setups where x11rb can't reach the display.
pub fn wake_display() {
    info!("WakeDisplay: Initiating display wake sequence (Linux)");
    let wayland = crate::linux_wayland::is_wayland_session();

    // On X11 use x11rb; on Wayland skip it (XWayland accepts DPMS requests but
    // they don't reach the real monitor) and use wlr-output-power.
    if !wayland && crate::linux_x11::wake() {
        info!("WakeDisplay: woke via x11");
        return;
    }
    if crate::linux_wayland::set_dpms(true) {
        info!("WakeDisplay: woke via wlr");
        return;
    }

    // Fallbacks: X11 tools (X11 only) + GNOME/KDE screensaver un-blank.
    if !wayland {
        let _ = Command::new("xdotool").args(["key", "shift"]).status();
        let _ = Command::new("xset").args(["dpms", "force", "on"]).status();
    }
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

/// Turn the display off (bundled X11 DPMS on X11 / wlr on Wayland, `xset` fallback).
pub fn monitor_off() {
    info!("MonitorOff: turning display off (Linux)");
    let wayland = crate::linux_wayland::is_wayland_session();
    if !wayland && crate::linux_x11::set_dpms(false) {
        return;
    }
    if crate::linux_wayland::set_dpms(false) {
        return;
    }
    if !wayland {
        let _ = Command::new("xset").args(["dpms", "force", "off"]).status();
    }
}
