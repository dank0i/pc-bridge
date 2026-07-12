//! Idle time sensor for Linux - tracks last user input and screensaver state
//!
//! Screensaver detection uses D-Bus org.freedesktop.ScreenSaver or xdg-screensaver.
//! Last-active time uses bundled backends (x11rb on X11, D-Bus via zbus on
//! GNOME/KDE Wayland), falling back to xprintidle/qdbus if those don't answer.

use log::{debug, info, warn};
use std::process::Command;
use std::sync::Arc;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

/// Format an OffsetDateTime as RFC 3339 string
fn format_rfc3339(dt: OffsetDateTime) -> String {
    dt.format(&Rfc3339).unwrap_or_else(|_| dt.to_string())
}

pub struct IdleSensor {
    state: Arc<AppState>,
}

impl IdleSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        let mut interval_secs = config.intervals.last_active.max(1); // Prevent panic on 0
        drop(config);

        // On wlroots Wayland (no D-Bus idle), start the ext-idle-notify listener
        // that backs the D-Bus-less idle path. Idempotent; the runtime
        // is_wayland_session() guard means it's a no-op off Wayland.
        if crate::linux_wayland::is_wayland_session() {
            crate::linux_idle::ensure_started();
        }

        let mut tick = interval(Duration::from_secs(interval_secs));
        // Skip missed ticks so a stall (e.g. after resume) doesn't fire a burst
        // of catch-up xprintidle subprocesses.
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

        // Track previous values to skip duplicate publishes. idle_seconds grows
        // each tick while idle (keeps publishing); lastactive freezes while idle.
        let mut prev_last_active = String::new();
        let mut prev_idle_secs: i64 = -1;

        // Publish initial state
        self.publish_idle(&mut prev_last_active, &mut prev_idle_secs)
            .await;

        // Publish initial screensaver state (retained so HA picks it up).
        // Off the runtime: is_screensaver_active spawns dbus-send subprocesses.
        let screensaver_active = tokio::task::spawn_blocking(is_screensaver_active)
            .await
            .unwrap_or(false);
        let screensaver_state = if screensaver_active { "on" } else { "off" };
        debug!("Initial screensaver state: {}", screensaver_state);
        self.state
            .mqtt
            .publish_sensor_retained("screensaver", screensaver_state)
            .await;
        let mut prev_screensaver_state = screensaver_active;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Idle sensor shutting down");
                    break;
                }
                // Config hot-reload: update poll interval
                Ok(()) = config_rx.recv() => {
                    let config = self.state.config.read().await;
                    let new_interval = config.intervals.last_active.max(1);
                    if new_interval != interval_secs {
                        interval_secs = new_interval;
                        tick = interval(Duration::from_secs(interval_secs));
                        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                        info!("Idle sensor: interval changed to {}s", interval_secs);
                    }
                }
                // MQTT reconnected: force republish current state
                Ok(()) = reconnect_rx.recv() => {
                    info!("Idle sensor: MQTT reconnected, republishing current state");
                    prev_last_active.clear();
                    prev_idle_secs = -1;
                    self.publish_idle(&mut prev_last_active, &mut prev_idle_secs).await;
                }
                _ = tick.tick() => {
                    self.publish_idle(&mut prev_last_active, &mut prev_idle_secs).await;

                    // Check screensaver state (polled alongside idle time), off
                    // the runtime: is_screensaver_active spawns dbus-send.
                    let screensaver_active = tokio::task::spawn_blocking(is_screensaver_active)
                        .await
                        .unwrap_or(false);
                    if screensaver_active != prev_screensaver_state {
                        let state_str = if screensaver_active { "on" } else { "off" };
                        debug!("Screensaver state changed: {}", state_str);
                        self.state.mqtt.publish_sensor_retained("screensaver", state_str).await;
                        prev_screensaver_state = screensaver_active;
                    }
                }
            }
        }
    }

    /// Publish `lastactive` + `idle_seconds` (parity with the Windows sensor).
    /// Skips publishing entirely when idle detection is unavailable, so we never
    /// fabricate "active now" (which would make the PC look perpetually busy and
    /// break "idle for N minutes" automations).
    async fn publish_idle(&self, prev_last: &mut String, prev_idle: &mut i64) {
        let Some(idle_secs) = self.get_idle_seconds().await else {
            return;
        };
        // idle_seconds grows while idle → publish on change (each tick).
        if idle_secs != *prev_idle {
            self.state
                .mqtt
                .publish_sensor("idle_seconds", &idle_secs.to_string())
                .await;
            *prev_idle = idle_secs;
        }
        // lastactive = now - idle → freezes while idle (stops publishing).
        // Quantized to the minute: while active it otherwise advances every tick
        // and republishes each poll; HA's idle/presence logic only needs minute
        // resolution and the string gate below then holds within the minute.
        let last_active = OffsetDateTime::now_utc() - time::Duration::seconds(idle_secs);
        let la_minute = last_active.unix_timestamp().div_euclid(60);
        let quantized = OffsetDateTime::from_unix_timestamp(la_minute * 60).unwrap_or(last_active);
        let formatted = format_rfc3339(quantized);
        if formatted != *prev_last {
            self.state
                .mqtt
                .publish_sensor("lastactive", &formatted)
                .await;
            *prev_last = formatted;
        }
    }

    /// Seconds since the last user input, or `None` if no detection method is
    /// available. Tries bundled x11rb (X11) then D-Bus (Wayland), then the
    /// external `xprintidle`/`qdbus` fallbacks.
    async fn get_idle_seconds(&self) -> Option<i64> {
        tokio::task::spawn_blocking(Self::get_idle_seconds_blocking)
            .await
            .ok()
            .flatten()
    }

    fn get_idle_seconds_blocking() -> Option<i64> {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::time::Instant;

        // Warn-once latch + failure backoff so an unsupported setup doesn't warn
        // every tick and keep forking xprintidle/qdbus forever. After 3 consecutive
        // full-probe failures we stop probing and retry only every 5 minutes (env
        // changes like logging into a session can make detection start working, so
        // we never give up permanently).
        static WARN_LOGGED: AtomicBool = AtomicBool::new(false);
        static FAIL_STREAK: AtomicU32 = AtomicU32::new(0);
        static BACKOFF_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
        const FAIL_THRESHOLD: u32 = 3;
        const BACKOFF: Duration = Duration::from_mins(5);

        // Skip all probes while backing off. Clear the window when it elapses so
        // the next tick runs one retry.
        {
            let mut until = BACKOFF_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = *until {
                if t > Instant::now() {
                    return None;
                }
                *until = None;
            }
        }

        let idle = Self::probe_idle_seconds();
        if idle.is_some() {
            FAIL_STREAK.store(0, Ordering::Relaxed);
            WARN_LOGGED.store(false, Ordering::Relaxed);
            return idle;
        }

        // Full-probe failure.
        let streak = FAIL_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
        if !WARN_LOGGED.swap(true, Ordering::Relaxed) {
            warn!("No idle time detection available (install xprintidle for X11)");
        }
        if streak >= FAIL_THRESHOLD {
            *BACKOFF_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + BACKOFF);
        }
        None
    }

    /// Run the idle-detection probes in priority order, returning the first hit.
    fn probe_idle_seconds() -> Option<i64> {
        // On Wayland, XWayland's screensaver counter doesn't track Wayland-native
        // input, so the X11 paths (x11rb/xprintidle) would report the user as idle
        // while active. Use the compositor's D-Bus idle there; on X11, x11rb is
        // exact.
        let wayland = crate::linux_wayland::is_wayland_session();

        // Bundled X11 (pure Rust) - X11 sessions only.
        if !wayland && let Some(ms) = crate::linux_x11::idle_millis() {
            return Some((ms / 1000) as i64);
        }

        // Bundled D-Bus (GNOME Mutter / KDE) - the Wayland path, and a fallback on
        // X11 GNOME/KDE too.
        if let Some(ms) = crate::linux_dbus::idle_millis() {
            return Some((ms / 1000) as i64);
        }

        // ext-idle-notify (wlroots: Sway/Hyprland) - a background listener keeps
        // this current; covers Wayland compositors with no D-Bus idle interface.
        if let Some(ms) = crate::linux_idle::idle_millis() {
            return Some((ms / 1000) as i64);
        }

        // xprintidle (X11 only): idle time in milliseconds.
        if !wayland
            && let Ok(output) = Command::new("xprintidle").output()
            && output.status.success()
            && let Ok(idle_str) = String::from_utf8(output.stdout)
            && let Ok(idle_ms) = idle_str.trim().parse::<i64>()
        {
            return Some(idle_ms / 1000);
        }

        // qdbus (KDE/Wayland): idle time in seconds.
        if let Ok(output) = Command::new("qdbus")
            .args([
                "org.freedesktop.ScreenSaver",
                "/ScreenSaver",
                "GetSessionIdleTime",
            ])
            .output()
            && output.status.success()
            && let Ok(idle_str) = String::from_utf8(output.stdout)
            && let Ok(idle_secs) = idle_str.trim().parse::<i64>()
        {
            return Some(idle_secs);
        }

        // No detection available: return None rather than fabricating now(). The
        // caller handles warn-once + backoff so we don't spam or keep forking.
        None
    }
}

/// Check if a screensaver is currently active on Linux.
///
/// Tries multiple detection methods:
/// 1. xdg-screensaver status (most portable)
/// 2. D-Bus org.freedesktop.ScreenSaver.GetActive (GNOME/KDE)
/// 3. D-Bus org.gnome.ScreenSaver.GetActive (GNOME-specific)
fn is_screensaver_active() -> bool {
    // NOTE: `xdg-screensaver status` reports whether the screensaver is *enabled*
    // (allowed to run), NOT whether it is *currently showing*, so it is
    // intentionally not used here. The D-Bus GetActive calls below report the
    // real active state.

    // Try freedesktop D-Bus interface
    if let Ok(output) = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.ScreenSaver",
            "--type=method_call",
            "--print-reply",
            // Bound the wait: a hung ScreenSaver service must not block the idle
            // tick (and process shutdown) for dbus-send's 25s default.
            "--reply-timeout=2000",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver.GetActive",
        ])
        .output()
        && output.status.success()
        && let Ok(reply) = String::from_utf8(output.stdout)
    {
        return reply.contains("boolean true");
    }

    // Try GNOME-specific D-Bus interface
    if let Ok(output) = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.gnome.ScreenSaver",
            "--type=method_call",
            "--print-reply",
            // Bound the wait: a hung ScreenSaver service must not block the idle
            // tick (and process shutdown) for dbus-send's 25s default.
            "--reply-timeout=2000",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver.GetActive",
        ])
        .output()
        && output.status.success()
        && let Ok(reply) = String::from_utf8(output.stdout)
    {
        return reply.contains("boolean true");
    }

    false
}
