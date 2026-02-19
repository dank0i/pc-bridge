//! Game detection sensor for Linux - monitors running processes
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)

use log::{debug, error};
use std::fs;
use std::sync::Arc;
use tokio::time::{Duration, interval};

use crate::AppState;

/// Cached lowered game patterns — rebuilt only when config changes via config_generation.
struct CachedGamePatterns {
    /// (lowered_pattern, game_id, display_name)
    patterns: Vec<(String, String, String)>,
}

impl CachedGamePatterns {
    fn build(games: &std::collections::HashMap<String, crate::config::GameConfig>) -> Self {
        let patterns = games
            .iter()
            .map(|(pattern, gc)| {
                (
                    pattern.to_lowercase(),
                    gc.game_id().to_string(),
                    gc.display_name(),
                )
            })
            .collect();
        Self { patterns }
    }
}

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
        let mut cached = CachedGamePatterns::build(&config.games);
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // Publish initial state
        let (game_id, display_name) = self.detect_game(&cached).await;
        self.publish_game(&game_id, &display_name).await;

        // Track last published state to avoid duplicate MQTT messages
        let mut last_game_id = game_id;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Game sensor shutting down");
                    break;
                }
                // Rebuild cached patterns when config changes
                Ok(()) = config_rx.recv() => {
                    let config = self.state.config.read().await;
                    cached = CachedGamePatterns::build(&config.games);
                    drop(config);
                    debug!("Game sensor: rebuilt cached patterns");
                    let (game_id, display_name) = self.detect_game(&cached).await;
                    if game_id != last_game_id {
                        self.publish_game(&game_id, &display_name).await;
                        last_game_id = game_id;
                    }
                }
                _ = tick.tick() => {
                    let (game_id, display_name) = self.detect_game(&cached).await;
                    if game_id != last_game_id {
                        self.publish_game(&game_id, &display_name).await;
                        last_game_id = game_id;
                    }
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

    async fn detect_game(&self, cached: &CachedGamePatterns) -> (String, String) {
        // Enumerate processes via /proc
        let processes = match self.get_process_names().await {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to enumerate processes: {}", e);
                return ("none".to_string(), "None".to_string());
            }
        };

        let mut found_games: Vec<(String, String)> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for proc_name in &processes {
            let proc_lower = proc_name.to_lowercase();

            for (pattern_lower, game_id, display_name) in &cached.patterns {
                if proc_lower.contains(pattern_lower.as_str()) {
                    // Avoid duplicates (same game matched by multiple processes)
                    if !seen_ids.contains(game_id) {
                        seen_ids.insert(game_id.clone());
                        found_games.push((game_id.clone(), display_name.clone()));
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

    async fn get_process_names(&self) -> anyhow::Result<Vec<String>> {
        // /proc enumeration is hundreds of blocking fs::read_to_string calls —
        // run off the single-threaded runtime.
        tokio::task::spawn_blocking(Self::get_process_names_blocking)
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {}", e))?
    }

    fn get_process_names_blocking() -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();

        // Read /proc to enumerate processes
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let path = entry.path();

            // Only process numeric directories (PIDs)
            if let Some(name) = path.file_name()
                && let Some(name_str) = name.to_str()
                && name_str.chars().all(|c| c.is_ascii_digit())
            {
                // Read the process command line or comm
                let comm_path = path.join("comm");
                if let Ok(comm) = fs::read_to_string(&comm_path) {
                    let comm = comm.trim().to_string();
                    if !comm.is_empty() {
                        names.push(comm);
                    }
                }
            }
        }

        Ok(names)
    }
}
