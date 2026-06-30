//! Now Playing (media session) sensor (Windows).
//!
//! Reads the active System Media Transport Controls session (the same source
//! that drives the Windows media flyout) and publishes "playing: Artist -
//! Title" / "paused: ..." / "idle" to the `now_playing` sensor.

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

pub struct NowPlayingSensor {
    state: Arc<AppState>,
}

impl NowPlayingSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev = String::new();

        info!("Now playing sensor started (polled every 5s)");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Now playing sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev.clear();
                }
                _ = tick.tick() => {
                    // The WinRT calls block on async .get(), so run them off the
                    // async runtime on a blocking thread.
                    let now = tokio::task::spawn_blocking(current_now_playing)
                        .await
                        .unwrap_or_else(|_| "idle".to_string());
                    if now != prev {
                        self.state
                            .mqtt
                            .publish_sensor_retained("now_playing", &now)
                            .await;
                        prev = now;
                    }
                }
            }
        }
    }
}

/// Query the current media session. Returns "idle" when nothing is playing or
/// the media APIs are unavailable.
fn current_now_playing() -> String {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    unsafe {
        // WinRT async waits need an initialized (MTA) apartment. Harmless if the
        // blocking-pool thread is already initialized.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let inner = || -> windows_core::Result<String> {
        let manager = Manager::RequestAsync()?.get()?;
        let session = manager.GetCurrentSession()?;
        let status = session.GetPlaybackInfo()?.PlaybackStatus()?;
        let props = session.TryGetMediaPropertiesAsync()?.get()?;
        let title = props.Title()?.to_string();
        let artist = props.Artist()?.to_string();

        let label = match (artist.trim().is_empty(), title.trim().is_empty()) {
            (false, false) => format!("{} - {}", artist.trim(), title.trim()),
            (true, false) => title.trim().to_string(),
            (false, true) => artist.trim().to_string(),
            (true, true) => return Ok("idle".to_string()),
        };
        let prefix = if status == Status::Playing {
            "playing"
        } else if status == Status::Paused {
            "paused"
        } else {
            "stopped"
        };
        Ok(format!("{prefix}: {label}"))
    };

    inner().unwrap_or_else(|_| "idle".to_string())
}
