//! Power event listener for Linux - detects sleep/wake via D-Bus signals
//!
//! Uses `gdbus monitor` on org.freedesktop.login1 to receive PrepareForSleep
//! signals instantly, matching the Windows WM_POWERBROADCAST behavior at zero
//! CPU cost when idle (blocks on pipe read, no polling).
//!
//! Also publishes a `display` sensor for real monitor DPMS power (polled via
//! `xset q`), matching the Windows `GUID_CONSOLE_DISPLAY_STATE` behavior. X11
//! only; on Wayland the sensor doesn't update.

use log::{debug, error, info, warn};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::AppState;

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

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Channel for events from blocking D-Bus reader threads
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<PowerEvent>(8);

        // Spawn blocking thread for sleep/wake events (system bus)
        let sleep_tx = event_tx.clone();
        match std::thread::Builder::new()
            .name("power-dbus".into())
            .stack_size(128 * 1024)
            .spawn(move || Self::dbus_sleep_monitor_thread(sleep_tx))
        {
            Ok(_) => {}
            Err(e) => {
                error!("Failed to spawn dbus sleep monitor thread: {}", e);
                return;
            }
        }

        // Spawn blocking thread for monitor DPMS power (xset poll)
        let display_tx = event_tx;
        match std::thread::Builder::new()
            .name("display-dpms".into())
            .stack_size(128 * 1024)
            .spawn(move || Self::dpms_poll_thread(display_tx))
        {
            Ok(_) => {}
            Err(e) => {
                // Non-fatal: display tracking is nice-to-have
                warn!("Failed to spawn dbus display monitor thread: {}", e);
            }
        }

        info!("Power event listener started (D-Bus monitor mode)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Power listener shutting down");
                    break;
                }
                Some(event) = event_rx.recv() => {
                    match event {
                        PowerEvent::Sleep => {
                            info!("Power event: SLEEP");
                            self.state.mqtt.publish_sensor_retained("sleep_state", "sleeping").await;
                        }
                        PowerEvent::Wake => {
                            info!("Power event: WAKE");
                            self.state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
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
    fn dbus_sleep_monitor_thread(tx: tokio::sync::mpsc::Sender<PowerEvent>) {
        loop {
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
                    Self::poll_fallback(&tx);
                    return;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    warn!("gdbus monitor has no stdout - falling back to polling");
                    Self::poll_fallback(&tx);
                    return;
                }
            };

            let reader = BufReader::new(stdout);
            for line in reader.lines() {
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
                    let _ = child.kill();
                    return;
                }
            }

            // gdbus exited unexpectedly - restart after a short delay
            let _ = child.wait();
            warn!("gdbus monitor exited, restarting in 2s");
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    /// Fallback: poll systemctl every 5 seconds (used if gdbus is unavailable)
    fn poll_fallback(tx: &tokio::sync::mpsc::Sender<PowerEvent>) {
        warn!("Using polling fallback for power events (install gdbus for instant detection)");
        let mut was_sleeping = false;

        loop {
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

            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    /// Blocking thread: polls monitor DPMS power via `xset q` and emits
    /// DisplayOn/DisplayOff on change, so the `display` sensor means real monitor
    /// power (matching the Windows GUID_CONSOLE_DISPLAY_STATE behavior) rather
    /// than screensaver state - so an idle DPMS-off, and the MonitorOff command
    /// (`xset dpms force off`), both flip it. X11-only: on Wayland `xset` fails
    /// and the sensor simply doesn't update.
    fn dpms_poll_thread(tx: tokio::sync::mpsc::Sender<PowerEvent>) {
        info!("Display state monitor started (xset DPMS poll)");
        let mut prev_on: Option<bool> = None;

        loop {
            // `xset q` prints a line like "Monitor is On" / "Monitor is Off" /
            // "Monitor is in Standby" / "Monitor is in Suspend".
            let on = match Command::new("xset").arg("q").output() {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    text.lines()
                        .find(|l| l.trim_start().starts_with("Monitor is"))
                        .map(|l| l.trim_end().ends_with("On"))
                }
                // xset unavailable (Wayland / no X11) or DPMS off: don't publish.
                _ => None,
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

            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
}
