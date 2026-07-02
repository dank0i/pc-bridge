//! Live Steam download progress (percentage), published as `steam_download`.
//!
//! The steamclient call itself runs in an ISOLATED subprocess
//! (`pc-bridge --steam-download-probe`, see [`crate::steam::download_probe`]) so a
//! private-ABI drift after a Steam update can crash only a throwaway process, never
//! the agent. This sensor spawns that probe ONLY while a download is actually active
//! (a cheap `.acf` check gates it), parses its one-line JSON, and publishes.
//!
//! `steam_updating` (on/off, from `.acf`, no setup) is separate; this is the % and
//! is opt-in (`features.steam_download_progress`).

use std::sync::Arc;
use std::time::Duration;

use log::debug;
use serde_json::json;
use tokio::time::{MissedTickBehavior, interval};

use crate::AppState;

pub struct SteamDownloadsSensor {
    state: Arc<AppState>,
}

impl SteamDownloadsSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let interval_secs = self
            .state
            .config
            .read()
            .await
            .intervals
            .steam_download
            .max(1);
        let mut tick = interval(Duration::from_secs(interval_secs));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        debug!("Steam download sensor started (isolated probe)");
        let mut prev = String::new();

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Steam download sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => { prev.clear(); }
                _ = tick.tick() => {
                    let (state_str, attrs) = self.poll().await;
                    // Key on state + attributes so a change in app_id/bytes (even at
                    // the same rounded percent) still republishes, and idle/unavailable
                    // don't spam.
                    let sig = format!("{state_str}|{attrs}");
                    if sig != prev {
                        self.state
                            .mqtt
                            .publish_sensor_retained("steam_download", &state_str)
                            .await;
                        self.state
                            .mqtt
                            .publish_sensor_attributes("steam_download", &attrs)
                            .await;
                        prev = sig;
                    }
                }
            }
        }
    }

    async fn poll(&self) -> (String, serde_json::Value) {
        // Cheap gate: only pay for the isolated probe when a download is actually
        // active (small .acf reads, no DLL load). Keeps the agent idle-cheap.
        let active = tokio::task::spawn_blocking(super::steam::download_in_progress)
            .await
            .unwrap_or(false);
        if !active {
            return ("0".to_string(), json!({ "state": "idle" }));
        }

        match run_probe().await {
            Some(Report::Downloading {
                appid,
                downloaded,
                total,
            }) => {
                let pct = ((downloaded as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
                (
                    format!("{pct:.0}"),
                    json!({
                        "state": "downloading",
                        "app_id": appid,
                        "percent": pct,
                        "bytes_downloaded": downloaded,
                        "bytes_total": total,
                    }),
                )
            }
            Some(Report::Idle) => ("0".to_string(), json!({ "state": "idle" })),
            // Probe couldn't reach the client / crashed (e.g. ABI drift): fail safe.
            None => ("unavailable".to_string(), json!({ "state": "unavailable" })),
        }
    }
}

enum Report {
    Downloading {
        appid: u32,
        downloaded: u64,
        total: u64,
    },
    Idle,
}

/// Spawn the isolated probe subprocess and parse its one-line JSON output.
async fn run_probe() -> Option<Report> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--steam-download-probe").kill_on_drop(true);

    // Let the probe resolve steamclient's sibling dependency libraries.
    if let Some(dir) = crate::steam::download_probe::steamclient_dir() {
        prepend_lib_search_path(&mut cmd, &dir);
    }
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .ok()?
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.trim();
    if line.is_empty() {
        return None; // no output -> couldn't reach the client
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("idle").and_then(serde_json::Value::as_bool) == Some(true) {
        return Some(Report::Idle);
    }
    let appid = v.get("appid")?.as_u64()? as u32;
    let downloaded = v.get("downloaded")?.as_u64()?;
    let total = v.get("total")?.as_u64()?;
    if total == 0 {
        return Some(Report::Idle);
    }
    Some(Report::Downloading {
        appid,
        downloaded,
        total,
    })
}

/// Prepend `dir` to the platform's dynamic-library search path for the child, so
/// steamclient's dependent libraries load.
fn prepend_lib_search_path(cmd: &mut tokio::process::Command, dir: &std::path::Path) {
    let (var, sep) = if cfg!(windows) {
        ("PATH", ';')
    } else if cfg!(target_os = "macos") {
        ("DYLD_LIBRARY_PATH", ':')
    } else {
        ("LD_LIBRARY_PATH", ':')
    };
    let existing = std::env::var(var).unwrap_or_default();
    let combined = if existing.is_empty() {
        dir.display().to_string()
    } else {
        format!("{}{}{}", dir.display(), sep, existing)
    };
    cmd.env(var, combined);
}
