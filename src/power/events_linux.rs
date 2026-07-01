//! Power event listener for Linux - detects sleep/wake via D-Bus signals
//!
//! Uses `gdbus monitor` on org.freedesktop.login1 to receive PrepareForSleep
//! signals instantly, matching the Windows WM_POWERBROADCAST behavior at zero
//! CPU cost when idle (blocks on pipe read, no polling).
//!
//! Also publishes a `display` sensor for real monitor DPMS power (polled via
//! bundled x11rb), matching the Windows `GUID_CONSOLE_DISPLAY_STATE` behavior.
//! X11 only; on Wayland the sensor doesn't update.

use log::{debug, error, info, warn};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::AppState;
use crate::power::sync_mqtt::{SyncMqttConfig, parse_broker_url, sync_mqtt_publish_sleep};

/// Power-related events from D-Bus monitor threads
enum PowerEvent {
    Sleep,
    Wake,
    DisplayOff,
    DisplayOn,
}

pub struct PowerEventListener {
    state: Arc<AppState>,
}

impl PowerEventListener {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Take a systemd-logind "sleep" delay-inhibitor. While the returned fd is
    /// held, logind waits (up to InhibitDelayMaxSec, ~5s) after PrepareForSleep
    /// before actually suspending, giving us a window to publish "sleeping"
    /// before the NIC drops. Dropping the fd releases the lock. `None` if logind
    /// isn't reachable (non-systemd system).
    fn take_sleep_inhibitor() -> Option<zbus::zvariant::OwnedFd> {
        let conn = zbus::blocking::Connection::system().ok()?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Inhibit",
                &(
                    "sleep",
                    "pc-bridge",
                    "Publish sleep state before suspend",
                    "delay",
                ),
            )
            .ok()?;
        reply.body().deserialize::<zbus::zvariant::OwnedFd>().ok()
    }

    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        let mut shutdown_rx = shutdown.subscribe();

        // Shutdown wiring for the two blocking OS threads. `stop` is polled by the
        // dpms thread (sliced sleep) and checked by the sleep thread between
        // gdbus restarts; `gdbus_child` lets us kill the in-flight `gdbus monitor`
        // to unblock its `reader.lines()`, which is otherwise parked forever. This
        // matters because the listener is now supervised: feature-disable drops
        // event_rx, and without this the threads + gdbus child would leak (and a
        // re-enable would stack a second set).
        let stop = Arc::new(AtomicBool::new(false));
        let gdbus_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

        // Hold a delay-inhibitor so an externally initiated suspend (power
        // button, lid, auto-sleep) still gets "sleeping" on the wire before the
        // NIC drops - mirroring the Windows sync-before-suspend path. Released on
        // the Sleep event (after the sync publish) and re-armed on Wake.
        let mut sleep_inhibitor = tokio::task::spawn_blocking(Self::take_sleep_inhibitor)
            .await
            .ok()
            .flatten();
        if sleep_inhibitor.is_some() {
            info!("Holding systemd sleep delay-inhibitor for pre-suspend publish");
        }

        // Channel for events from blocking D-Bus reader threads
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<PowerEvent>(8);

        // Spawn blocking thread for sleep/wake events (system bus)
        let sleep_tx = event_tx.clone();
        let sleep_stop = Arc::clone(&stop);
        let sleep_child = Arc::clone(&gdbus_child);
        match std::thread::Builder::new()
            .name("power-dbus".into())
            .stack_size(128 * 1024)
            .spawn(move || Self::dbus_sleep_monitor_thread(&sleep_tx, &sleep_stop, &sleep_child))
        {
            Ok(_) => {}
            Err(e) => {
                error!("Failed to spawn dbus sleep monitor thread: {}", e);
                return;
            }
        }

        // Spawn blocking thread for monitor DPMS power (x11rb/wlr poll)
        let display_tx = event_tx;
        let dpms_stop = Arc::clone(&stop);
        match std::thread::Builder::new()
            .name("display-dpms".into())
            .stack_size(128 * 1024)
            .spawn(move || Self::dpms_poll_thread(&display_tx, &dpms_stop))
        {
            Ok(_) => {}
            Err(e) => {
                // Non-fatal: display tracking is nice-to-have
                warn!("Failed to spawn dbus display monitor thread: {}", e);
            }
        }

        // Independent shutdown waiter: sets the stop flag and kills+reaps the gdbus
        // child on shutdown. It's a SEPARATE task (not the run() shutdown arm) so it
        // still fires if the supervisor aborts run() while it's parked in the Sleep
        // arm's sync-publish await - otherwise that abort would skip cleanup and leak
        // the two threads + the gdbus child (and re-enable would stack a second set).
        {
            let mut wait_rx = shutdown.subscribe();
            let waiter_stop = Arc::clone(&stop);
            let waiter_child = Arc::clone(&gdbus_child);
            tokio::spawn(async move {
                let _ = wait_rx.recv().await;
                waiter_stop.store(true, Ordering::Relaxed);
                if let Some(mut c) = waiter_child.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            });
        }

        info!("Power event listener started (D-Bus monitor mode)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Power listener shutting down");
                    // The independent waiter above sets stop + reaps the child; just
                    // exit the loop here (idempotent if the waiter already ran).
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                Some(event) = event_rx.recv() => {
                    match event {
                        PowerEvent::Sleep => {
                            info!("Power event: SLEEP");
                            // Guaranteed-delivery sync publish (fresh TCP) before we
                            // release the inhibitor and the system suspends. Offloaded
                            // so the blocking connect doesn't stall the runtime.
                            let cfg = {
                                let config = self.state.config.read().await;
                                let (host, port, use_tls) = parse_broker_url(&config.mqtt.broker);
                                SyncMqttConfig {
                                    host,
                                    port,
                                    use_tls,
                                    user: config.mqtt.user.clone(),
                                    pass: config.mqtt.pass.clone(),
                                    client_id: format!("{}-sleep", config.client_id()),
                                    sleep_topic: format!(
                                        "homeassistant/sensor/{}/sleep_state/state",
                                        config.device_name
                                    ),
                                }
                            };
                            match tokio::task::spawn_blocking(move || sync_mqtt_publish_sleep(&cfg))
                                .await
                            {
                                Ok(Ok(())) => info!("Sleep state pre-published via sync TCP"),
                                Ok(Err(e)) => warn!("Sync MQTT sleep pre-publish failed: {}", e),
                                Err(e) => warn!("Sync publish task join error: {}", e),
                            }
                            self.state.mqtt.publish_sensor_retained("sleep_state", "sleeping").await;
                            // Drop the fd to release the delay-inhibitor: logind now
                            // proceeds to suspend.
                            drop(sleep_inhibitor.take());
                        }
                        PowerEvent::Wake => {
                            info!("Power event: WAKE");
                            self.state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
                            // Re-arm the inhibitor for the next suspend, off the
                            // runtime (the D-Bus connect+call is blocking).
                            sleep_inhibitor = tokio::task::spawn_blocking(Self::take_sleep_inhibitor)
                                .await
                                .ok()
                                .flatten();
                        }
                        PowerEvent::DisplayOff => {
                            info!("Power event: DISPLAY OFF");
                            self.state.mqtt.publish_sensor_retained("display", "off").await;
                        }
                        PowerEvent::DisplayOn => {
                            info!("Power event: DISPLAY ON");
                            self.state.mqtt.publish_sensor_retained("display", "on").await;
                        }
                    }
                }
            }
        }
    }

    /// Blocking thread: runs `gdbus monitor` and parses PrepareForSleep signals.
    ///
    /// Signal format:
    /// `/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (true)`
    /// `/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (false)`
    fn dbus_sleep_monitor_thread(
        tx: &tokio::sync::mpsc::Sender<PowerEvent>,
        stop: &AtomicBool,
        child_slot: &Mutex<Option<Child>>,
    ) {
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let child = Command::new("gdbus")
                .args([
                    "monitor",
                    "--system",
                    "--dest",
                    "org.freedesktop.login1",
                    "--object-path",
                    "/org/freedesktop/login1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "Failed to start gdbus monitor: {} - falling back to polling",
                        e
                    );
                    Self::poll_fallback(tx, stop);
                    return;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    warn!("gdbus monitor has no stdout - falling back to polling");
                    Self::poll_fallback(tx, stop);
                    return;
                }
            };

            // Publish the child so run() can kill it on shutdown (unblocks the
            // reader below). We keep ownership via the shared slot.
            *child_slot.lock().unwrap() = Some(child);

            // Close the race where disable fired between spawn and store: run()
            // would have taken None and set stop, and the reader below could block
            // forever. Re-check and self-reap.
            if stop.load(Ordering::Relaxed) {
                if let Some(mut c) = child_slot.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                return;
            }

            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(line) = line else { break };

                // Look for PrepareForSleep signal
                if !line.contains("PrepareForSleep") {
                    continue;
                }

                let going_to_sleep = line.contains("true");
                let event = if going_to_sleep {
                    PowerEvent::Sleep
                } else {
                    PowerEvent::Wake
                };
                if tx.blocking_send(event).is_err() {
                    // Receiver dropped (shutdown)
                    if let Some(mut c) = child_slot.lock().unwrap().take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    return;
                }
            }

            // Reader ended: either run() killed the child on shutdown, or gdbus
            // exited on its own. Reap it and decide whether to restart.
            if let Some(mut c) = child_slot.lock().unwrap().take() {
                let _ = c.wait();
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            warn!("gdbus monitor exited, restarting in 2s");
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    /// Fallback: poll systemctl every 5 seconds (used if gdbus is unavailable)
    fn poll_fallback(tx: &tokio::sync::mpsc::Sender<PowerEvent>, stop: &AtomicBool) {
        warn!("Using polling fallback for power events (install gdbus for instant detection)");
        let mut was_sleeping = false;

        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let is_sleeping = if let Ok(output) = Command::new("systemctl")
                .args(["is-active", "sleep.target"])
                .output()
                && let Ok(state) = String::from_utf8(output.stdout)
            {
                state.trim() == "active"
            } else {
                false
            };

            if is_sleeping != was_sleeping {
                let event = if is_sleeping {
                    PowerEvent::Sleep
                } else {
                    PowerEvent::Wake
                };
                if tx.blocking_send(event).is_err() {
                    return;
                }
                was_sleeping = is_sleeping;
            }

            // Sleep ~5s in slices so disable/shutdown is observed promptly.
            for _ in 0..50 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    /// Blocking thread: polls monitor DPMS power via bundled x11rb (X11) or wlr
    /// (wlroots Wayland) and emits DisplayOn/DisplayOff on change, so the
    /// `display` sensor means real monitor power (matching the Windows
    /// GUID_CONSOLE_DISPLAY_STATE behavior) rather than screensaver state - so an
    /// idle DPMS-off, and the MonitorOff command, both flip it. On GNOME/KDE
    /// Wayland there's no query path, so the sensor simply doesn't update.
    fn dpms_poll_thread(tx: &tokio::sync::mpsc::Sender<PowerEvent>, stop: &AtomicBool) {
        info!("Display state monitor started (x11rb/wlr DPMS poll)");
        let mut prev_on: Option<bool> = None;

        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            // On Wayland use wlr (XWayland's DPMS is its own, not the real
            // monitor); on X11 use x11rb. None on GNOME/KDE Wayland (no wlr) -
            // the sensor simply doesn't update there.
            let on = if crate::linux_wayland::is_wayland_session() {
                crate::linux_wayland::dpms_on()
            } else {
                crate::linux_x11::dpms_on()
            };

            if let Some(on) = on
                && prev_on != Some(on)
            {
                prev_on = Some(on);
                let event = if on {
                    PowerEvent::DisplayOn
                } else {
                    PowerEvent::DisplayOff
                };
                if tx.blocking_send(event).is_err() {
                    return; // receiver dropped (shutdown)
                }
            }

            // Sleep ~5s in slices so disable/shutdown is observed promptly.
            for _ in 0..50 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}
