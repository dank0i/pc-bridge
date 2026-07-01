//! Pure-Rust X11 backend (via `x11rb`) for idle time, active window, monitor
//! DPMS power, and input injection - so the X11 path needs no external tools
//! (`xdotool`/`xset`/`xprintidle`/`xprop`).
//!
//! Compiles on any unix (x11rb is a unix dependency), but only connects on an
//! actual X11 session; every entry point returns `None`/does nothing when no X11
//! display is reachable (Wayland or headless), which is how callers fall back.

use x11rb::connection::Connection;
use x11rb::protocol::dpms::{self, ConnectionExt as DpmsExt};
use x11rb::protocol::screensaver::ConnectionExt as ScreenSaverExt;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as XProtoExt};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;

/// Milliseconds since the last user input on the default X11 display, or `None`
/// if no X11 display is reachable.
pub fn idle_millis() -> Option<u64> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;
    let info = conn.screensaver_query_info(root).ok()?.reply().ok()?;
    Some(u64::from(info.ms_since_user_input))
}

/// Title of the currently-focused window (`_NET_ACTIVE_WINDOW` + `_NET_WM_NAME`,
/// falling back to `WM_NAME`), or `None` if unavailable.
pub fn active_window_title() -> Option<String> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;

    let net_active = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let reply = conn
        .get_property(false, root, net_active, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let win = reply.value32()?.next()?;
    if win == 0 {
        return None;
    }

    let net_wm_name = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if let Ok(cookie) = conn.get_property(false, win, net_wm_name, utf8, 0, 1024)
        && let Ok(name) = cookie.reply()
        && !name.value.is_empty()
    {
        return String::from_utf8(name.value).ok();
    }

    // Legacy WM_NAME (Latin-1).
    let wm_name = conn
        .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if wm_name.value.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&wm_name.value).into_owned())
}

/// Monitor DPMS power: `Some(true)` if on, `Some(false)` if off/standby/suspend,
/// `None` if no X11 display.
pub fn dpms_on() -> Option<bool> {
    let (conn, _) = x11rb::connect(None).ok()?;
    let info = conn.dpms_info().ok()?.reply().ok()?;
    // DPMS disabled means no power management, so the monitor is on.
    if !info.state {
        return Some(true);
    }
    Some(info.power_level == dpms::DPMSMode::ON)
}

/// Force the monitor DPMS power on or off. Returns whether the request was sent.
pub fn set_dpms(on: bool) -> bool {
    let Ok((conn, _)) = x11rb::connect(None) else {
        return false;
    };
    // ForceLevel requires DPMS enabled.
    let _ = conn.dpms_enable();
    let level = if on {
        dpms::DPMSMode::ON
    } else {
        dpms::DPMSMode::OFF
    };
    let ok = conn.dpms_force_level(level).is_ok();
    let _ = conn.flush();
    ok
}

/// Wake the display: force DPMS on and fake a benign key press/release (XTEST)
/// to reset the idle timer. Returns whether an X11 display was reached.
pub fn wake() -> bool {
    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return false;
    };
    let _ = conn.dpms_enable();
    let _ = conn.dpms_force_level(dpms::DPMSMode::ON);
    // Fake Shift press+release (keycode 50 = Shift_L on standard layouts); a
    // no-op for the user but enough to reset the idle/screensaver timer.
    if let Some(root) = conn.setup().roots.get(screen_num).map(|s| s.root) {
        let _ = conn.xtest_fake_input(2, 50, 0, root, 0, 0, 0); // KeyPress
        let _ = conn.xtest_fake_input(3, 50, 0, root, 0, 0, 0); // KeyRelease
    }
    let _ = conn.flush();
    true
}
