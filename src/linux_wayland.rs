//! Bundled Wayland (wlr protocols) backend for active window and monitor DPMS
//! on wlroots compositors (Sway, Hyprland, gaming stacks). Everything here is
//! best-effort: any connection/protocol failure returns `None`/does nothing, so
//! a non-wlroots session (GNOME/KDE) or missing protocol just falls through to
//! "unavailable" - it can never break anything.

// The wayland-rs Dispatch impls take unit / &unit params idiomatically.
#![allow(clippy::ignored_unit_patterns)]

use std::collections::HashMap;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};

// ── Active window via wlr-foreign-toplevel-management ──────────────────────

#[derive(Default)]
struct ToplevelState {
    /// object id -> (title, is-activated)
    windows: HashMap<ObjectId, (String, bool)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ToplevelState {
    fn event(
        _state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == ZwlrForeignToplevelManagerV1::interface().name
        {
            registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(name, 1, qh, ());
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for ToplevelState {
    fn event(
        _state: &mut Self,
        _mgr: &ZwlrForeignToplevelManagerV1,
        _event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The `toplevel` event delivers a new handle; wayland-rs routes its
        // events to the ZwlrForeignToplevelHandleV1 Dispatch below (event-created
        // child with () user-data).
    }

    wayland_client::event_created_child!(ToplevelState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for ToplevelState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.windows.entry(id).or_default().0 = title;
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: s } => {
                // `s` is a byte buffer of u32 state values; Activated == 2.
                let activated = s
                    .chunks_exact(4)
                    .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == 2);
                state.windows.entry(id).or_default().1 = activated;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.windows.remove(&id);
            }
            _ => {}
        }
    }
}

/// Title of the activated toplevel on a wlroots compositor, or `None`.
pub fn active_window_title() -> Option<String> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<ToplevelState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = ToplevelState::default();
    // A couple of roundtrips: bind the manager, then receive the toplevel list
    // and their title/state events.
    for _ in 0..3 {
        queue.roundtrip(&mut state).ok()?;
    }

    state
        .windows
        .values()
        .find(|(_, activated)| *activated)
        .map(|(title, _)| title.clone())
        .filter(|t| !t.is_empty())
}

/// Whether this session exposes the wlr-foreign-toplevel protocol (wlroots).
#[cfg(target_os = "linux")]
pub fn has_foreign_toplevel() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let mut queue = conn.new_event_queue::<ToplevelState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = ToplevelState::default();
    // One roundtrip binds the manager if the global is advertised.
    queue.roundtrip(&mut state).is_ok()
}

// ── Monitor DPMS via wlr-output-power-management ───────────────────────────

#[derive(Default)]
struct OutputPowerState {
    manager: Option<ZwlrOutputPowerManagerV1>,
    outputs: Vec<wayland_client::protocol::wl_output::WlOutput>,
    /// per output-power object: is-on
    modes: HashMap<ObjectId, bool>,
    controls: Vec<ZwlrOutputPowerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for OutputPowerState {
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
            if interface == ZwlrOutputPowerManagerV1::interface().name {
                state.manager =
                    Some(registry.bind::<ZwlrOutputPowerManagerV1, _, _>(name, 1, qh, ()));
            } else if interface == wayland_client::protocol::wl_output::WlOutput::interface().name {
                let output = registry.bind::<wayland_client::protocol::wl_output::WlOutput, _, _>(
                    name,
                    1,
                    qh,
                    (),
                );
                state.outputs.push(output);
            }
        }
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for OutputPowerState {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputPowerManagerV1,
        _: <ZwlrOutputPowerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_output::WlOutput, ()> for OutputPowerState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_output::WlOutput,
        _: <wayland_client::protocol::wl_output::WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for OutputPowerState {
    fn event(
        state: &mut Self,
        control: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_power_v1::Event::Mode { mode } = event {
            let on = matches!(
                mode,
                wayland_client::WEnum::Value(zwlr_output_power_v1::Mode::On)
            );
            state.modes.insert(control.id(), on);
        }
    }
}

fn output_power_connect() -> Option<(
    Connection,
    wayland_client::EventQueue<OutputPowerState>,
    OutputPowerState,
)> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<OutputPowerState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = OutputPowerState::default();
    queue.roundtrip(&mut state).ok()?; // discover manager + outputs
    state.manager.as_ref()?;
    // Create a power-control for each output.
    let manager = state.manager.clone()?;
    for output in state.outputs.clone() {
        let control = manager.get_output_power(&output, &qh, ());
        state.controls.push(control);
    }
    queue.roundtrip(&mut state).ok()?; // receive Mode events
    Some((conn, queue, state))
}

/// Monitor DPMS power on wlroots: `Some(true)` if any output is on, `None` if
/// the protocol isn't available.
pub fn dpms_on() -> Option<bool> {
    let (_conn, _queue, state) = output_power_connect()?;
    if state.modes.is_empty() {
        return None;
    }
    Some(state.modes.values().any(|on| *on))
}

/// Set all outputs' DPMS power on/off on wlroots. Returns whether it applied.
pub fn set_dpms(on: bool) -> bool {
    let Some((conn, queue, state)) = output_power_connect() else {
        return false;
    };
    let mode = if on {
        zwlr_output_power_v1::Mode::On
    } else {
        zwlr_output_power_v1::Mode::Off
    };
    for control in &state.controls {
        control.set_mode(mode);
    }
    let _ = queue.flush();
    let _ = conn.flush();
    !state.controls.is_empty()
}

/// Whether this session exposes wlr-output-power (wlroots).
#[cfg(target_os = "linux")]
pub fn has_output_power() -> bool {
    output_power_connect().is_some()
}
