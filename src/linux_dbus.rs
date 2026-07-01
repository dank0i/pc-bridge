//! Bundled (pure-Rust, no external tools) D-Bus queries for Wayland where an
//! X11 path isn't available. Currently: idle time via GNOME Mutter's
//! `IdleMonitor` or KDE's `ScreenSaver`.

/// Milliseconds since last user input via D-Bus (GNOME Mutter / KDE), or `None`
/// if neither service answers. Works on Wayland and X11 GNOME/KDE sessions.
pub fn idle_millis() -> Option<u64> {
    let conn = zbus::blocking::Connection::session().ok()?;

    // GNOME Mutter IdleMonitor: GetIdletime -> milliseconds.
    if let Ok(reply) = conn.call_method(
        Some("org.gnome.Mutter.IdleMonitor"),
        "/org/gnome/Mutter/IdleMonitor/Core",
        Some("org.gnome.Mutter.IdleMonitor"),
        "GetIdletime",
        &(),
    ) && let Ok(ms) = reply.body().deserialize::<u64>()
    {
        return Some(ms);
    }

    // KDE ScreenSaver: GetSessionIdleTime -> seconds.
    if let Ok(reply) = conn.call_method(
        Some("org.freedesktop.ScreenSaver"),
        "/org/freedesktop/ScreenSaver",
        Some("org.freedesktop.ScreenSaver"),
        "GetSessionIdleTime",
        &(),
    ) && let Ok(secs) = reply.body().deserialize::<u32>()
    {
        return Some(u64::from(secs) * 1000);
    }

    None
}
