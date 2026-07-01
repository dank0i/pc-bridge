//! Pure-Rust X11 backend (via `x11rb`) for idle time, active window, monitor
//! DPMS power, and input injection - so the X11 path needs no external tools
//! (`xdotool`/`xset`/`xprintidle`/`xprop`).
//!
//! Compiles on any unix (x11rb is a unix dependency), but only connects on an
//! actual X11 session; every entry point returns `None`/does nothing when no X11
//! display is reachable (Wayland or headless), which is how callers fall back.

use x11rb::connection::Connection;
use x11rb::protocol::screensaver::ConnectionExt as ScreenSaverExt;

/// Milliseconds since the last user input on the default X11 display, or `None`
/// if no X11 display is reachable.
pub fn idle_millis() -> Option<u64> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;
    let info = conn.screensaver_query_info(root).ok()?.reply().ok()?;
    Some(u64::from(info.ms_since_user_input))
}
