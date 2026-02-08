//! Game detection sensor - monitors running processes to detect games
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)

use smallvec::SmallVec;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error};

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

        let poll_interval = Duration::from_secs(interval_secs);
        let mut tick = interval(poll_interval);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Publish initial state
        let (game_id, display_name) = self.detect_game(poll_interval).await;
        self.publish_game(&game_id, &display_name).await;

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Game sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let (game_id, display_name) = self.detect_game(poll_interval).await;
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

    async fn detect_game(&self, _max_cache_age: Duration) -> (String, String) {
        // Get processes from event-driven watcher (always up-to-date)
        let processes = self.state.process_watcher.get_names().await;

        // Check config games (includes Steam auto-discovered games)
        // Hold read lock while checking - faster than cloning
        let config = self.state.config.read().await;

        // Fix #6: SmallVec - stack-allocated for up to 4 concurrent games
        let mut found_games: SmallVec<[(String, String); 4]> = SmallVec::new();
        // Fix #2: Pre-allocate HashSet with expected capacity
        let mut seen_ids: HashSet<String> = HashSet::with_capacity(config.games.len());

        for proc_name in &processes {
            let proc_lower = proc_name.to_lowercase();
            let base_name = proc_lower.trim_end_matches(".exe");

            for (pattern, game_config) in &config.games {
                let pattern_lower = pattern.to_lowercase();
                if proc_lower.starts_with(&pattern_lower) || base_name == pattern_lower {
                    let game_id = game_config.game_id().to_string();
                    // Avoid duplicates (same game matched by multiple processes)
                    if seen_ids.insert(game_id.clone()) {
                        found_games.push((game_id, game_config.display_name()));
                    }
                }
            }
        }

        if found_games.is_empty() {
            ("none".to_string(), "None".to_string())
        } else {
            let ids: SmallVec<[&str; 4]> = found_games.iter().map(|(id, _)| id.as_str()).collect();
            let names: SmallVec<[&str; 4]> =
                found_games.iter().map(|(_, name)| name.as_str()).collect();
            (ids.join(","), names.join(","))
        }
    }
}
