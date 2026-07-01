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

#[cfg(unix)]
static PLAYERCTL_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    // Tab-delimited fields so empty artist/title can be collapsed the same way
    // the Windows branch does (a combined "artist - title" string would be
    // ambiguous when either half is empty).
    let out = match Command::new("playerctl")
        .args([
            "metadata",
            "--format",
            "{{lc(status)}}\t{{artist}}\t{{title}}",
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound
                && !PLAYERCTL_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                log::warn!(
                    "playerctl not found; now-playing will report 'idle' (install playerctl)"
                );
            }
            return "idle".to_string();
        }
    };
    if !out.status.success() {
        return "idle".to_string();
    }
    parse_playerctl(&String::from_utf8_lossy(&out.stdout))
}

/// Turn a tab-delimited "status\tartist\ttitle" line into a sensor value,
/// collapsing empty artist/title exactly like the Windows branch.
#[cfg(unix)]
fn parse_playerctl(raw: &str) -> String {
    let line = raw.trim_end_matches(['\n', '\r']);
    let mut parts = line.splitn(3, '\t');
    let status = parts.next().unwrap_or("").trim();
    let artist = parts.next().unwrap_or("").trim();
    let title = parts.next().unwrap_or("").trim();

    let label = match (artist.is_empty(), title.is_empty()) {
        (false, false) => format!("{artist} - {title}"),
        (true, false) => title.to_string(),
        (false, true) => artist.to_string(),
        (true, true) => return "idle".to_string(),
    };
    if status.is_empty() {
        "idle".to_string()
    } else {
        format!("{status}: {label}")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::parse_playerctl;

    #[test]
    fn test_parse_playerctl() {
        assert_eq!(
            parse_playerctl("playing\tQueen\tBohemian Rhapsody\n"),
            "playing: Queen - Bohemian Rhapsody"
        );
        // Empty artist / empty title collapse (no dangling " - ").
        assert_eq!(
            parse_playerctl("playing\t\tBohemian Rhapsody"),
            "playing: Bohemian Rhapsody"
        );
        assert_eq!(parse_playerctl("paused\tArtist\t"), "paused: Artist");
        // No metadata / no player -> idle.
        assert_eq!(parse_playerctl("playing\t\t"), "idle");
        assert_eq!(parse_playerctl(""), "idle");
    }
}
