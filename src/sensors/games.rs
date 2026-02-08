//! Game detection sensor - monitors running processes to detect games
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)

use serde_json;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::*;

use crate::AppState;

pub struct GameSensor {
    state: Arc<AppState>,
}

impl GameSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        let interval_secs = config.intervals.game_sensor.max(1); // Prevent panic on 0
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Publish initial state
        let (game_id, display_name) = self.detect_game().await;
        self.publish_game(&game_id, &display_name).await;

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Game sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let (game_id, display_name) = self.detect_game().await;
                    self.publish_game(&game_id, &display_name).await;
                }
            }
        }
    }

    async fn publish_game(&self, game_ids: &str, display_names: &str) {
        self.state
            .mqtt
            .publish_sensor("runninggames", game_ids)
            .await;
        let attrs = serde_json::json!({
            "display_name": display_names
        });
        self.state
            .mqtt
            .publish_sensor_attributes("runninggames", &attrs)
            .await;
    }

    async fn detect_game(&self) -> (String, String) {
        // Enumerate processes
        let processes = match self.get_process_names() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to enumerate processes: {}", e);
                return ("none".to_string(), "None".to_string());
            }
        };

        // Check config games (includes Steam auto-discovered games)
        // Hold read lock while checking - faster than cloning
        let config = self.state.config.read().await;

        let mut found_games: Vec<(String, String)> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for proc_name in &processes {
            let proc_lower = proc_name.to_lowercase();
            let base_name = proc_lower.trim_end_matches(".exe");

            for (pattern, game_config) in &config.games {
                let pattern_lower = pattern.to_lowercase();
                if proc_lower.starts_with(&pattern_lower) || base_name == pattern_lower {
                    let game_id = game_config.game_id().to_string();
                    // Avoid duplicates (same game matched by multiple processes)
                    if !seen_ids.contains(&game_id) {
                        seen_ids.insert(game_id.clone());
                        found_games.push((game_id, game_config.display_name()));
                    }
                }
            }
        }

        if found_games.is_empty() {
            ("none".to_string(), "None".to_string())
        } else {
            let ids: Vec<&str> = found_games.iter().map(|(id, _)| id.as_str()).collect();
            let names: Vec<&str> = found_games.iter().map(|(_, name)| name.as_str()).collect();
            (ids.join(","), names.join(","))
        }
    }

    fn get_process_names(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();

        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;

            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };

            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let name = String::from_utf16_lossy(&entry.szExeFile)
                        .trim_end_matches('\0')
                        .to_string();

                    if !name.is_empty() {
                        names.push(name);
                    }

                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }

            let _ = CloseHandle(snapshot);
        }

        Ok(names)
    }
}
