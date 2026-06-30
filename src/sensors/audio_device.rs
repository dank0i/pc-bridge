//! Default audio output device sensor (Windows).
//!
//! Polls the current default output device's friendly name and publishes it to
//! the `audio_device` sensor when it changes (e.g. switching headset/speakers).

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
        // The default device changes rarely; a light poll is plenty and avoids
        // hooking MQTT into the COM device-change callback.
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
                    let name = crate::audio::get_default_device_name()
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
