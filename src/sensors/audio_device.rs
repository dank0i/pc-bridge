//! Default audio output device sensor.
//!
//! Publishes the current default output device's friendly name to the
//! `audio_device` sensor when it changes.
//! - Windows: WASAPI default endpoint friendly name, re-read only when the
//!   device-change notification advances (via `audio::default_device_generation`)
//!   so a stable device costs nothing.
//! - Linux: PulseAudio/PipeWire default sink description (via `pactl`).

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::AppState;

#[cfg(unix)]
static PACTL_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct AudioDeviceSensor {
    state: Arc<AppState>,
}

impl AudioDeviceSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev = String::new();
        // Windows: last-seen device-change generation (None forces a read).
        #[cfg(windows)]
        let mut last_gen: Option<u64> = None;

        info!("Audio device sensor started");

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Audio device sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    #[cfg(windows)]
                    {
                        last_gen = None;
                    }
                    prev.clear();
                }
                _ = tick.tick() => {
                    // On Windows, skip the COM read unless the default device
                    // actually changed since last time. On Linux, poll each tick.
                    #[cfg(windows)]
                    {
                        let generation = crate::audio::default_device_generation();
                        if last_gen == Some(generation) {
                            continue;
                        }
                        last_gen = Some(generation);
                    }
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
    let default = run_pactl(&["get-default-sink"])?;
    let default = default.trim();
    if default.is_empty() {
        return None;
    }
    // Resolve a human-friendly description to match the Windows value; fall back
    // to the raw sink name if the description can't be found.
    if let Some(list) = run_pactl(&["list", "sinks"])
        && let Some(desc) = sink_description(&list, default)
    {
        return Some(desc);
    }
    Some(default.to_string())
}

#[cfg(unix)]
fn run_pactl(args: &[&str]) -> Option<String> {
    use std::process::Command;
    match Command::new("pactl").args(args).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(_) => None,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound
                && !PACTL_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                log::warn!(
                    "pactl not found; audio-device sensor will report 'unknown' (install pulseaudio-utils)"
                );
            }
            None
        }
    }
}

/// Extract the `Description:` of the sink whose `Name:` matches `sink_name` from
/// `pactl list sinks` output.
#[cfg(unix)]
fn sink_description(list: &str, sink_name: &str) -> Option<String> {
    let mut name_matches = false;
    for line in list.lines() {
        let t = line.trim();
        if t.starts_with("Sink #") {
            name_matches = false;
        } else if let Some(n) = t.strip_prefix("Name: ") {
            name_matches = n.trim() == sink_name;
        } else if let Some(d) = t.strip_prefix("Description: ")
            && name_matches
        {
            return Some(d.trim().to_string());
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::sink_description;

    #[test]
    fn test_sink_description() {
        let list = "Sink #0\n\tName: alsa_output.pci-0000_00.analog-stereo\n\tDescription: Built-in Audio\nSink #1\n\tName: bluez_output.AABBCC\n\tDescription: WH-1000XM4\n";
        assert_eq!(
            sink_description(list, "bluez_output.AABBCC"),
            Some("WH-1000XM4".to_string())
        );
        assert_eq!(
            sink_description(list, "alsa_output.pci-0000_00.analog-stereo"),
            Some("Built-in Audio".to_string())
        );
        assert_eq!(sink_description(list, "nonexistent"), None);
    }
}
