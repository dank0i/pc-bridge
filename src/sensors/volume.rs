//! System output volume sensor.
//!
//! Publishes the default device volume as a percentage (0-100) to the
//! `volume_level` sensor. Backs the "Volume" feature, which registered an
//! entity but previously had no producer. Uses `audio::get_volume` (WASAPI on
//! Windows, `pactl` on Linux).

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct VolumeSensor {
    state: Arc<AppState>,
}

impl VolumeSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev = String::new();

        info!("Volume sensor started (polled every 5s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Volume sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev.clear();
                }
                _ = tick.tick() => {
                    // get_volume blocks (COM on Windows, a subprocess on Linux),
                    // so keep it off the single-threaded async runtime.
                    let value = tokio::task::spawn_blocking(crate::audio::get_volume)
                        .await
                        .ok()
                        .flatten()
                        .map(|v| (v.round() as i64).clamp(0, 100).to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    if value != prev {
                        self.state.mqtt.publish_sensor("volume_level", &value).await;
                        prev = value;
                    }
                }
            }
        }
    }
}
