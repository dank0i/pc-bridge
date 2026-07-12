//! Display wake functions for Linux
#![allow(dead_code)] // Used on Linux only

use log::{info, warn};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// The display power state to seed at startup, or None when it can't be observed
/// on this stack (GNOME/KDE Wayland with no wlr power protocol). A None must seed
/// the sensor "unavailable" rather than a confident "on" that never updates -
/// MonitorOff is also a no-op there, so a persistent retained "on" is a lie.
pub fn observed_display_state() -> Option<&'static str> {
    let wayland = crate::linux_wayland::is_wayland_session();
    if !wayland && let Some(on) = crate::linux_x11::dpms_on() {
        return Some(if on { "on" } else { "off" });
    }
    crate::linux_wayland::dpms_on().map(|on| if on { "on" } else { "off" })
}

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
    if !wayland
        && Command::new("xset")
            .args(["dpms", "force", "off"])
            .status()
            .is_ok_and(|s| s.success())
    {
        return;
    }
    // Nothing worked (e.g. GNOME/KDE Wayland with no wlr power protocol). Warn
    // once instead of silently no-op'ing so the user knows the button did nothing.
    if !MONITOR_OFF_WARNED.swap(true, Ordering::Relaxed) {
        warn!("MonitorOff not supported on this display stack (no usable DPMS source)");
    }
}

/// Latch so an unsupported MonitorOff warns only once, not on every press.
static MONITOR_OFF_WARNED: AtomicBool = AtomicBool::new(false);
