//! Power event listener for Linux - detects sleep/wake via D-Bus signals
//!
//! Uses `gdbus monitor` on org.freedesktop.login1 to receive PrepareForSleep
//! signals instantly, matching the Windows WM_POWERBROADCAST behavior at zero
//! CPU cost when idle (blocks on pipe read, no polling).

use log::{debug, error, info, warn};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::AppState;

pub struct PowerEventListener {
    state: Arc<AppState>,
}

impl PowerEventListener {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Channel for events from the blocking gdbus reader thread
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<bool>(4);

        // Spawn blocking thread that reads gdbus monitor output line-by-line
        match std::thread::Builder::new()
            .name("power-dbus".into())
            .stack_size(128 * 1024)
            .spawn(move || Self::dbus_monitor_thread(event_tx))
        {
            Ok(_) => {}
            Err(e) => {
                error!("Failed to spawn dbus monitor thread: {}", e);
                return;
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
                Some(going_to_sleep) = event_rx.recv() => {
                    if going_to_sleep {
                        info!("Power event: SLEEP");
                        self.state.mqtt.publish_sensor_retained("sleep_state", "sleeping").await;
                    } else {
                        info!("Power event: WAKE");
                        self.state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
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
    fn dbus_monitor_thread(tx: tokio::sync::mpsc::Sender<bool>) {
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
                if tx.blocking_send(going_to_sleep).is_err() {
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
    fn poll_fallback(tx: &tokio::sync::mpsc::Sender<bool>) {
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
                if tx.blocking_send(is_sleeping).is_err() {
                    return;
                }
                was_sleeping = is_sleeping;
            }

            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
}
