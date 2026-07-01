//! Default audio output device sensor.
//!
//! Publishes the current default output device's friendly name to the
//! `audio_device` sensor when it changes.
//! - Windows: WASAPI default endpoint friendly name.
//! - Linux: PulseAudio/PipeWire default sink (via `pactl`).

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct AudioDeviceSensor {
    state: Arc<AppState>,
}

impl AudioDeviceSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        // The default device changes rarely; a light poll is plenty.
        let mut tick = interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev = String::new();

        info!("Audio device sensor started (polled every 15s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Audio device sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev.clear();
                }
                _ = tick.tick() => {
                    // The read blocks (COM on Windows, a subprocess on Linux), so
                    // keep it off the single-threaded async runtime.
                    let name = tokio::task::spawn_blocking(read_default_device)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "unknown".to_string());
                    if name != prev {
                        self.state
                            .mqtt
                            .publish_sensor_retained("audio_device", &name)
                            .await;
                        prev = name;
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn read_default_device() -> Option<String> {
    crate::audio::get_default_device_name()
}

#[cfg(unix)]
fn read_default_device() -> Option<String> {
    use std::process::Command;
    // PulseAudio / PipeWire report the default sink name; best-effort.
    let out = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}
