//! Physical monitor attach/detach sensor (Linux).
//!
//! Twin of the Windows `display_attached` sensor. Windows learns about monitor
//! arrival and removal from CfgMgr32 interface notifications; there is no
//! equivalent that works without a udev netlink socket here, so this polls DRM
//! connector state instead. That is cheap: a desktop exposes a handful of
//! connectors and each check is one small read per connector.
//!
//! `/sys/class/drm/*/status` is the right source rather than anything in X11 or
//! Wayland: it is the kernel's own view, so it answers correctly with no display
//! server running at all - which is exactly the headless case this sensor exists
//! to detect.
//!
//! Compiled on all of `cfg(unix)` so its parser is unit-tested on the
//! maintainer's Mac, but only SPAWNED on Linux (see `supervisor::TASKS`): macOS
//! has no `/sys/class/drm` and registers no entity for this feature.

#![cfg_attr(target_os = "macos", allow(dead_code))]

use log::{debug, info, warn};
use std::sync::Arc;
use std::time::Duration;

use crate::AppState;

const SENSOR: &str = "display_attached";
const DRM_ROOT: &str = "/sys/class/drm";

/// How often to re-read connector state. Monitor hotplug is a human-timescale
/// event, and each poll is a few tiny sysfs reads, so this trades a few seconds
/// of latency for not needing a udev socket.
const POLL: Duration = Duration::from_secs(5);

/// Latch so a permanently missing `/sys/class/drm` (a container, a VM with no
/// DRM driver) warns once instead of every poll. The Windows twin has the same
/// latch for the same reason; this one was written without it.
static DRM_READ_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct DisplayAttachedSensor {
    state: Arc<AppState>,
}

impl DisplayAttachedSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        let mut shutdown_rx = shutdown.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut tick = tokio::time::interval(POLL);
        // Dedup on the PUBLISHED STRING, not on a bool: that way the "unknown"
        // state dedups too, instead of republishing every tick while blind.
        let mut last: Option<&'static str> = None;

        info!("Display attached sensor started (DRM connector poll)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Display attached sensor shutting down");
                    break;
                }
                r = reconnect_rx.recv() => {
                    // Closed: `biased` re-polls this arm first every iteration
                    // and it is instantly ready, so falling through spins hot.
                    if !matches!(
                        r,
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) {
                        break;
                    }
                    // Force a republish: a broker that lost retained state has
                    // nothing, and hotplug may not happen again for days.
                    last = None;
                }
                _ = tick.tick() => {}
            }

            last = self.publish(last).await;
        }
    }

    /// Reads current state and publishes on change. Returns the new dedup value.
    async fn publish(&self, last: Option<&'static str>) -> Option<&'static str> {
        let value = match read_drm_attached() {
            Some(true) => "attached",
            Some(false) => "detached",
            // No readable connectors at all: a container without /sys, or a
            // machine with no DRM driver (some VMs). Report the shared unknown
            // sentinel rather than "detached", which would be a confident "no
            // monitor" from a producer that cannot see. Matches the Windows twin.
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

/// True if any DRM connector reports `connected`.
///
/// `None` means nothing could be read at all, which is NOT the same as "no
/// monitor": it is the unobservable case.
fn read_drm_attached() -> Option<bool> {
    read_drm_attached_in(std::path::Path::new(DRM_ROOT))
}

/// Split out so the parsing is testable against a fake sysfs tree.
fn read_drm_attached_in(root: &std::path::Path) -> Option<bool> {
    let Ok(entries) = std::fs::read_dir(root) else {
        if !DRM_READ_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            warn!(
                "Cannot read {}; display_attached will report unknown (further \
                 failures silent)",
                root.display()
            );
        }
        return None;
    };

    let mut any_readable = false;
    let mut attached = false;

    for entry in entries.flatten() {
        let status = entry.path().join("status");
        let Ok(contents) = std::fs::read_to_string(&status) else {
            // Not every entry under /sys/class/drm is a connector: the render
            // and card nodes have no `status` file. Skipping them is normal and
            // must not count as a failed read.
            continue;
        };
        any_readable = true;
        // The kernel writes "connected", "disconnected" or "unknown" plus a
        // newline. "unknown" means the driver could not probe THIS connector;
        // treat it as not attached rather than as global blindness, since other
        // connectors may still answer.
        if contents.trim().eq_ignore_ascii_case("connected") {
            attached = true;
            break;
        }
    }

    if any_readable { Some(attached) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn connector(root: &std::path::Path, name: &str, status: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), status).unwrap();
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("pcbridge-drm-{name}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn test_connected_connector_reads_attached() {
        let root = tmp("attached");
        connector(&root, "card1-DP-1", "connected\n");
        connector(&root, "card1-HDMI-A-1", "disconnected\n");
        assert_eq!(read_drm_attached_in(&root), Some(true));
    }

    #[test]
    fn test_all_disconnected_reads_detached() {
        let root = tmp("detached");
        connector(&root, "card1-DP-1", "disconnected\n");
        connector(&root, "card1-HDMI-A-1", "disconnected\n");
        assert_eq!(read_drm_attached_in(&root), Some(false));
    }

    /// The kernel's own third state. It means this connector could not be
    /// probed, not that the machine is blind, so other connectors still decide.
    #[test]
    fn test_connector_status_unknown_is_not_attached() {
        let root = tmp("connstatus-unknown");
        connector(&root, "card1-DP-1", "unknown\n");
        assert_eq!(read_drm_attached_in(&root), Some(false));

        connector(&root, "card1-DP-2", "connected\n");
        assert_eq!(read_drm_attached_in(&root), Some(true));
    }

    /// card0 / renderD128 have no `status` file. Skipping them is normal and
    /// must not be mistaken for an unreadable tree.
    #[test]
    fn test_non_connector_entries_are_skipped() {
        let root = tmp("noncon");
        fs::create_dir_all(root.join("card0")).unwrap();
        fs::create_dir_all(root.join("renderD128")).unwrap();
        assert_eq!(
            read_drm_attached_in(&root),
            None,
            "no connectors at all is unobservable, not detached"
        );

        connector(&root, "card0-DP-1", "disconnected\n");
        assert_eq!(read_drm_attached_in(&root), Some(false));
    }

    #[test]
    fn test_missing_drm_root_is_unobservable() {
        let root = std::env::temp_dir().join("pcbridge-drm-does-not-exist");
        let _ = fs::remove_dir_all(&root);
        assert_eq!(read_drm_attached_in(&root), None);
    }

    #[test]
    fn test_status_is_case_and_whitespace_tolerant() {
        let root = tmp("case");
        connector(&root, "card1-DP-1", "  Connected  ");
        assert_eq!(read_drm_attached_in(&root), Some(true));
    }
}
