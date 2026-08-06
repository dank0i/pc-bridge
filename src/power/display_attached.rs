//! Physical display attach/detach sensor (Windows).
//!
//! Deliberately separate from the `display` sensor. That one tracks display
//! POWER via `GUID_CONSOLE_DISPLAY_STATE` (on/off/dimmed). This one answers a
//! different question: is a real monitor physically CONNECTED. The two are not
//! interchangeable - a headless host with no monitor at all still reports
//! display power `on`, so `display` cannot be used to detect headlessness.
//!
//! Event-driven via `CM_Register_Notification` on `GUID_DEVINTERFACE_MONITOR`.
//! No polling, no timer, no window and no message pump: the OS calls us only
//! when a monitor interface actually arrives or disappears, so the idle cost is
//! zero.
//!
//! The CfgMgr32 callback runs on a system-owned thread and must not block or
//! re-enter CfgMgr32 (doing so risks deadlocking device installation), so it
//! does nothing but signal a channel. Enumeration and publishing happen on the
//! async side.
//!
//! Virtual displays are excluded, otherwise an active remote-desktop session
//! that spins up a virtual monitor would read as "a monitor is attached" and
//! invert the answer for exactly the headless case this sensor exists to detect.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_NOTIFY_ACTION, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CM_Unregister_Notification,
    CR_SUCCESS, HCMNOTIFICATION,
};
use windows::Win32::Devices::Display::GUID_DEVINTERFACE_MONITOR;
use windows::core::PCWSTR;

use crate::AppState;

/// Sensor name, and therefore the MQTT topic leaf. Distinct from `display`.
const SENSOR: &str = "display_attached";

/// Device-instance substrings identifying VIRTUAL displays, which must not count
/// as a physically attached monitor. Matched case-insensitively against each
/// monitor interface's symbolic link.
///
/// `PSCCDD0` is Parsec's virtual display adapter (ParsecVDA). The others are the
/// common indirect-display drivers people pair with streaming hosts. Extending
/// this list is the supported way to teach the sensor about a new VDD.
const VIRTUAL_DISPLAY_MARKERS: &[&str] =
    &["PSCCDD0", "IDDSAMPLEDRIVER", "USBMMIDD", "VIRTUALDISPLAY"];

/// Hotplug emits several interface notifications in a burst (the monitor, its
/// child nodes, then the topology change). Coalesce them so one physical plug
/// produces one MQTT publish rather than three or four.
const DEBOUNCE: Duration = Duration::from_millis(750);

/// Latches so a persistently failing enumeration warns once rather than either
/// spamming the log or (at debug level, which is below the default) saying
/// nothing at all while the sensor sits frozen on a stale value.
static ENUM_FAILURE_WARNED: AtomicBool = AtomicBool::new(false);

/// Owns the CfgMgr32 registration and the callback context, tearing both down
/// in the correct order on drop. RAII rather than manual cleanup so an early
/// return or a panic still unregisters.
struct Registration {
    hnotify: HCMNOTIFICATION,
    ctx: *mut mpsc::UnboundedSender<()>,
}

// SAFETY: `hnotify` is an opaque OS handle CfgMgr32 permits using from any
// thread, and `ctx` is only ever dereferenced by the callback (invoked by the OS
// on its own thread) and freed in Drop after the registration is torn down. The
// async task never dereferences either, so moving the pair across threads is
// sound - which is what lets `run()` be a Send future.
unsafe impl Send for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: unregister FIRST so no callback can still be in flight, only
        // then free the context that callback would have dereferenced. The
        // reverse order is a use-after-free.
        unsafe {
            let _ = CM_Unregister_Notification(self.hnotify);
            drop(Box::from_raw(self.ctx));
        }
    }
}

pub struct DisplayAttachedSensor {
    state: Arc<AppState>,
}

impl DisplayAttachedSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Takes the per-task shutdown SENDER (not a receiver) to match the other
    /// resource-holding sensors: the supervisor fires it to stop us, and we must
    /// unregister the OS notification before returning.
    pub async fn run(self, shutdown: broadcast::Sender<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();

        // Register inside a block so the raw locals (`ctx`, `hnotify`) die here.
        // Only `Registration` - which is explicitly Send - crosses an await, which
        // is what keeps this future spawnable on the multi-thread runtime.
        // Returns Option so the raw locals (`ctx`, `hnotify`) die inside this
        // block. Nothing may await in here: a live raw pointer across an await
        // makes the whole future !Send and the supervisor cannot spawn it.
        let registration = {
            // Context must outlive the registration; ownership passes to
            // Registration, whose Drop frees it after unregistering.
            let ctx = Box::into_raw(Box::new(tx));

            let mut filter = CM_NOTIFY_FILTER {
                cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
                FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
                ..Default::default()
            };
            filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_MONITOR;

            let mut hnotify = HCMNOTIFICATION::default();
            // SAFETY: `filter` is fully initialized and outlives the call; `ctx`
            // is a live Box pointer whose ownership moves into Registration.
            let cr = unsafe {
                CM_Register_Notification(
                    &raw const filter,
                    Some(ctx as *const c_void),
                    Some(notify_callback),
                    &raw mut hnotify,
                )
            };

            if cr == CR_SUCCESS {
                Some(Registration { hnotify, ctx })
            } else {
                error!(
                    "CM_Register_Notification for monitor interfaces failed: {:?} - \
                     display_attached will not update",
                    cr
                );
                // SAFETY: registration failed, so no callback can be in flight
                // and no Registration exists to free this.
                drop(unsafe { Box::from_raw(ctx) });
                None
            }
        };

        let Some(registration) = registration else {
            // Publish a value anyway: the entity is registered, and with no
            // notification nothing else will ever write it.
            self.state
                .mqtt
                .publish_sensor(SENSOR, crate::mqtt::SENTINEL_UNKNOWN)
                .await;
            // Then PARK rather than return. Returning leaves a finished handle the
            // supervisor cannot tell from a crash, so it respawns on escalating
            // backoff forever and inflates bridge_health's restart count - the same
            // ping-pong sensors/steam.rs was fixed for.
            let mut sd = shutdown.subscribe();
            let _ = sd.recv().await;
            return;
        };

        info!("Display attach/detach listener started (event-driven, CfgMgr32)");

        // Seed once so HA has a value before the first hotplug.
        let mut last = self.publish(None).await;

        let mut sd = shutdown.subscribe();
        // A broker restart without persistence drops our retained state topic, and
        // this sensor is purely event-driven: with no monitor hotplug it would
        // never republish, leaving the entity permanently unknown in HA on exactly
        // the headless machines it exists for. Clear `last` so the next publish is
        // unconditional.
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        loop {
            tokio::select! {
                // Shutdown must win: the notification arm below holds a 750ms
                // debounce sleep, so without `biased` a hotplug burst arriving
                // at shutdown can push exit past the supervisor's per-task
                // budget. Every other long-lived select in the repo is biased.
                biased;
                _ = sd.recv() => break,
                r = reconnect_rx.recv() => {
                    // Treat Lagged as a reconnect; Closed means the sender is gone.
                    if !matches!(
                        r,
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) {
                        break;
                    }
                    last = None;
                    last = self.publish(last).await;
                }
                recv = rx.recv() => {
                    if recv.is_none() {
                        break;
                    }
                    // Coalesce the burst, then drain whatever else piled up.
                    tokio::time::sleep(DEBOUNCE).await;
                    while rx.try_recv().is_ok() {}
                    last = self.publish(last).await;
                }
            }
        }

        // Explicit, not relying on the `_registration` binding name: renaming it
        // to `_` (a plausible "unused variable" cleanup) would drop it HERE at the
        // top of the function instead, after which every hotplug callback writes
        // through a freed Box for the life of the process.
        drop(registration);
        debug!("Display attach/detach listener stopped");
    }

    /// Enumerate and publish, but only when the answer actually changed.
    /// Returns the value now believed current, for the next comparison.
    async fn publish(&self, last: Option<&'static str>) -> Option<&'static str> {
        // Dedup on the PUBLISHED STRING, matching the Linux twin. Dedupping on
        // Option<bool> meant a failed probe published "unknown" and then
        // returned the UNCHANGED `last`, so the next failed probe published
        // "unknown" all over again.
        //
        // A failed probe must publish rather than stay silent: a retained value
        // from a previous run would otherwise stand, and on a now-headless box
        // that reads `attached`.
        //
        // Off the runtime: CM_Get_Device_Interface_List* are blocking syscalls,
        // matching how every other Windows syscall here is handled.
        let probed = tokio::task::spawn_blocking(physical_display_present)
            .await
            .unwrap_or(None);
        let value = match probed {
            Some(true) => "attached",
            Some(false) => "detached",
            None => crate::mqtt::SENTINEL_UNKNOWN,
        };

        if last == Some(value) {
            return last;
        }
        self.state.mqtt.publish_sensor(SENSOR, value).await;
        debug!("display_attached -> {}", value);
        Some(value)
    }
}

/// CfgMgr32 notification callback.
///
/// Runs on a system thread. Must not block, allocate heavily, or call back into
/// CfgMgr32, so it only nudges the async side. The return value is a Win32
/// error code; 0 is ERROR_SUCCESS.
unsafe extern "system" fn notify_callback(
    _hnotify: HCMNOTIFICATION,
    context: *const c_void,
    _action: CM_NOTIFY_ACTION,
    _eventdata: *const CM_NOTIFY_EVENT_DATA,
    _eventdatasize: u32,
) -> u32 {
    if !context.is_null() {
        // SAFETY: `context` is the Box<Sender> pointer handed to
        // CM_Register_Notification, which stays alive until after
        // CM_Unregister_Notification returns.
        let tx = unsafe { &*context.cast::<mpsc::UnboundedSender<()>>() };
        // Send failure just means the async side already shut down.
        let _ = tx.send(());
    }
    0
}

/// Warn on the first enumeration failure only. A frozen sensor is otherwise
/// invisible: the value silently stops updating and debug-level logging is below
/// the default level, so nothing would ever say why.
fn warn_enum_failure(api: &str, cr: impl std::fmt::Debug) {
    if ENUM_FAILURE_WARNED.swap(true, Ordering::Relaxed) {
        debug!("{api} failed: {cr:?}");
    } else {
        warn!("{api} failed: {cr:?} - display_attached will hold its last value");
    }
}

/// True when at least one NON-VIRTUAL monitor interface is present.
///
/// `None` on enumeration failure, so callers can hold the previous value rather
/// than publishing a confident wrong answer.
fn physical_display_present() -> Option<bool> {
    // Bind the const to a local: taking &raw const of a constant would point
    // at a temporary that does not outlive the FFI call.
    let guid = GUID_DEVINTERFACE_MONITOR;
    let mut len: u32 = 0;
    // SAFETY: out-param is a live local; the GUID is a 'static const.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            &raw const guid,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        warn_enum_failure("CM_Get_Device_Interface_List_SizeW", cr);
        return None;
    }
    if len <= 1 {
        // Empty multi-sz (just the terminator): nothing attached at all.
        return Some(false);
    }

    let mut buf = vec![0u16; len as usize];
    // SAFETY: buffer is sized exactly as the size query requested.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            &raw const guid,
            PCWSTR::null(),
            &mut buf,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        warn_enum_failure("CM_Get_Device_Interface_ListW", cr);
        return None;
    }

    // Multi-sz: NUL-separated, double-NUL terminated.
    let any_physical = buf
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .any(|link| {
            let upper = link.to_ascii_uppercase();
            !VIRTUAL_DISPLAY_MARKERS
                .iter()
                .any(|marker| upper.contains(marker))
        });

    Some(any_physical)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker match is what keeps a live Parsec session from reading as a
    /// physical monitor, so pin the real symbolic-link shape observed on the
    /// host rather than a synthetic string.
    #[test]
    fn parsec_vdd_link_is_recognized_as_virtual() {
        let link =
            r"\\?\DISPLAY#PSCCDD0#1&28a6823a&0&UID256_0#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        let upper = link.to_ascii_uppercase();
        assert!(
            VIRTUAL_DISPLAY_MARKERS.iter().any(|m| upper.contains(m)),
            "Parsec VDD must be classified virtual"
        );
    }

    #[test]
    fn real_monitor_link_is_not_virtual() {
        let link =
            r"\\?\DISPLAY#AUS27D2#5&1a2b3c4d&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        let upper = link.to_ascii_uppercase();
        assert!(
            !VIRTUAL_DISPLAY_MARKERS.iter().any(|m| upper.contains(m)),
            "a physical monitor must not be classified virtual"
        );
    }

    #[test]
    fn markers_are_uppercase_so_matching_works() {
        // physical_display_present() uppercases the link before comparing, so a
        // lowercase marker here would silently never match.
        for m in VIRTUAL_DISPLAY_MARKERS {
            assert_eq!(*m, m.to_ascii_uppercase(), "marker must be uppercase");
        }
    }
}
