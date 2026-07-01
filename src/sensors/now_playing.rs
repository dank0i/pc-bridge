//! Now Playing (media session) sensor.
//!
//! Publishes "playing: Artist - Title" / "paused: ..." / "idle" to the
//! `now_playing` sensor.
//! - Windows: System Media Transport Controls (GSMTC).
//! - Linux: MPRIS via `playerctl`.

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
                    // WinRT .get() and the Linux subprocess both block, so run
                    // them off the single-threaded async runtime.
                    let now = tokio::task::spawn_blocking(read_now_playing)
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
#[cfg(windows)]
fn read_now_playing() -> String {
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

/// MPRIS via `playerctl`. Returns "idle" when no player is active.
#[cfg(unix)]
fn read_now_playing() -> String {
    use std::process::Command;
    let out = Command::new("playerctl")
        .args([
            "metadata",
            "--format",
            "{{lc(status)}}: {{artist}} - {{title}}",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => parse_playerctl(&String::from_utf8_lossy(&o.stdout)),
        _ => "idle".to_string(),
    }
}

/// Turn a `playerctl` line into a sensor value. A player with no metadata yields
/// e.g. "playing:  - "; any result whose artist/title part has no real content
/// becomes "idle".
#[cfg(unix)]
fn parse_playerctl(raw: &str) -> String {
    let s = raw.trim();
    let has_content = s
        .split_once(": ")
        .map_or("", |(_, rest)| rest)
        .chars()
        .any(|c| c.is_alphanumeric());
    if has_content {
        s.to_string()
    } else {
        "idle".to_string()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::parse_playerctl;

    #[test]
    fn test_parse_playerctl() {
        assert_eq!(
            parse_playerctl("playing: Queen - Bohemian Rhapsody\n"),
            "playing: Queen - Bohemian Rhapsody"
        );
        assert_eq!(parse_playerctl("paused: Artist - "), "paused: Artist -");
        // No metadata / no player -> idle.
        assert_eq!(parse_playerctl("playing:  - "), "idle");
        assert_eq!(parse_playerctl("stopped: -"), "idle");
        assert_eq!(parse_playerctl(""), "idle");
    }
}
