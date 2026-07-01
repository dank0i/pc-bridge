//! Bundled idle detection for wlroots Wayland compositors (Sway, Hyprland, ...)
//! via the `ext-idle-notify-v1` protocol. Unlike GNOME/KDE these expose no D-Bus
//! `GetIdletime`, and the protocol is event-based (idled/resumed at a threshold)
//! rather than a query, so a background thread holds the connection and
//! translates events into an "idle since" timestamp the sensor polls.
//!
//! Best-effort: if the protocol/seat isn't present the listener marks itself
//! unavailable and `idle_millis()` returns `None`, so callers fall through.

#![allow(clippy::ignored_unit_patterns)]

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

/// The idle-notification threshold. `idled` fires after this long with no input;
/// small so the reported idle value starts climbing quickly.
const IDLE_THRESHOLD_MS: u32 = 1000;

struct Shared {
    /// True once the listener bound the notifier and is dispatching.
    available: bool,
    /// When the current idle period began, or `None` while the user is active.
    idle_since: Option<Instant>,
    /// True once the background thread has been spawned (spawn-once guard).
    spawned: bool,
}

static IDLE: OnceLock<Mutex<Shared>> = OnceLock::new();

fn shared() -> &'static Mutex<Shared> {
    IDLE.get_or_init(|| {
        Mutex::new(Shared {
            available: false,
            idle_since: None,
            spawned: false,
        })
    })
}

/// Milliseconds since last input from the ext-idle-notify listener, or `None` if
/// the protocol isn't available on this compositor / the listener isn't running.
pub fn idle_millis() -> Option<u64> {
    let s = shared().lock().ok()?;
    if !s.available {
        return None;
    }
    Some(s.idle_since.map_or(0, |t| {
        t.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }))
}

/// Start the background ext-idle-notify listener once. Safe to call repeatedly;
/// only the first call spawns the thread. A no-op-ish on compositors without the
/// protocol (the thread exits and leaves `available = false`).
pub fn ensure_started() {
    {
        let mut s = shared().lock().unwrap_or_else(|e| e.into_inner());
        if s.spawned {
            return;
        }
        s.spawned = true;
    }
    let _ = std::thread::Builder::new()
        .name("wl-idle-notify".into())
        .spawn(listen);
}

fn set_available(available: bool) {
    if let Ok(mut s) = shared().lock() {
        s.available = available;
        if !available {
            s.idle_since = None;
        }
    }
}

fn set_idle(idle: bool) {
    if let Ok(mut s) = shared().lock() {
        s.idle_since = if idle {
            // `idled` fires after IDLE_THRESHOLD_MS of inactivity, so the idle
            // period actually began that long ago.
            Some(
                Instant::now()
                    .checked_sub(Duration::from_millis(u64::from(IDLE_THRESHOLD_MS)))
                    .unwrap_or_else(Instant::now),
            )
        } else {
            None
        };
    }
}

#[derive(Default)]
struct Listener {
    notifier: Option<ExtIdleNotifierV1>,
    seat: Option<WlSeat>,
}

fn listen() {
    let Ok(conn) = Connection::connect_to_env() else {
        return; // not a Wayland session
    };
    let mut queue = conn.new_event_queue::<Listener>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut listener = Listener::default();
    if queue.roundtrip(&mut listener).is_err() {
        return;
    }
    let (Some(notifier), Some(seat)) = (listener.notifier.clone(), listener.seat.clone()) else {
        return; // no ext-idle-notify / seat on this compositor
    };

    // Create the notification; idled/resumed then arrive on its Dispatch.
    let _notification = notifier.get_idle_notification(IDLE_THRESHOLD_MS, &seat, &qh, ());
    let _ = conn.flush();

    set_available(true);
    // Block dispatching events until the connection dies (compositor exit).
    while queue.blocking_dispatch(&mut listener).is_ok() {}
    set_available(false);
}

impl Dispatch<wl_registry::WlRegistry, ()> for Listener {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            if interface == ExtIdleNotifierV1::interface().name {
                state.notifier = Some(registry.bind::<ExtIdleNotifierV1, _, _>(name, 1, qh, ()));
            } else if interface == WlSeat::interface().name {
                state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
            }
        }
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => set_idle(true),
            ext_idle_notification_v1::Event::Resumed => set_idle(false),
            _ => {}
        }
    }
}
