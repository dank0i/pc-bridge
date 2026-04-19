//! Power event listener for Linux - detects sleep/wake via D-Bus signals
//!
//! Uses `gdbus monitor` on org.freedesktop.login1 to receive PrepareForSleep
//! signals instantly, matching the Windows WM_POWERBROADCAST behavior at zero
//! CPU cost when idle (blocks on pipe read, no polling).
//!
//! Also monitors display on/off via `org.freedesktop.ScreenSaver.ActiveChanged`
//! and `org.gnome.ScreenSaver.ActiveChanged` signals, publishing a `display`
//! sensor matching the Windows `GUID_CONSOLE_DISPLAY_STATE` behavior.

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

        // Spawn blocking thread for display on/off events (session bus)
        let display_tx = event_tx;
        match std::thread::Builder::new()
            .name("display-dbus".into())
            .stack_size(128 * 1024)
            .spawn(move || Self::dbus_display_monitor_thread(display_tx))
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
                        "Failed to start gdbus monitor: {} — falling back to polling",
                        e
                    );
                    Self::poll_fallback(&tx);
                    return;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    warn!("gdbus monitor has no stdout — falling back to polling");
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

            // gdbus exited unexpectedly — restart after a short delay
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

    /// Blocking thread: monitors screensaver state via D-Bus session bus.
    ///
    /// Watches `org.freedesktop.ScreenSaver.ActiveChanged` (KDE, XFCE) and
    /// `org.gnome.ScreenSaver.ActiveChanged` (GNOME) signals. The screensaver
    /// becoming active maps to display "off", and deactivation maps to "on".
    ///
    /// Signal format (both interfaces are identical):
    /// `ActiveChanged (true)`  → screensaver activated (display off)
    /// `ActiveChanged (false)` → screensaver deactivated (display on)
    fn dbus_display_monitor_thread(tx: tokio::sync::mpsc::Sender<PowerEvent>) {
        loop {
            // Monitor the session bus for screensaver signals from any sender.
            // We don't specify --dest so we catch both org.freedesktop.ScreenSaver
            // and org.gnome.ScreenSaver (whichever is present on this DE).
            let child = Command::new("dbus-monitor")
                .args([
                    "--session",
                    "type='signal',interface='org.freedesktop.ScreenSaver',member='ActiveChanged'",
                    "type='signal',interface='org.gnome.ScreenSaver',member='ActiveChanged'",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "Failed to start dbus-monitor for display state: {} — display sensor unavailable",
                        e
                    );
                    return;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    warn!("dbus-monitor has no stdout — display sensor unavailable");
                    return;
                }
            };

            info!("Display state monitor started (dbus-monitor session bus)");

            let reader = BufReader::new(stdout);
            // dbus-monitor outputs multi-line blocks; ActiveChanged signal is followed
            // by a line like "   boolean true" or "   boolean false"
            let mut expect_boolean = false;

            for line in reader.lines() {
                let Ok(line) = line else { break };

                if line.contains("ActiveChanged") {
                    expect_boolean = true;
                    continue;
                }

                if expect_boolean {
                    expect_boolean = false;
                    let trimmed = line.trim();
                    let event = if trimmed.contains("true") {
                        PowerEvent::DisplayOff
                    } else if trimmed.contains("false") {
                        PowerEvent::DisplayOn
                    } else {
                        continue;
                    };
                    if tx.blocking_send(event).is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }

            let _ = child.wait();
            warn!("dbus-monitor for display exited, restarting in 2s");
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}
