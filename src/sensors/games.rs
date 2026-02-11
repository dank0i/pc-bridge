//! Game detection sensor - monitors running processes to detect games
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)
//!
//! Uses push notifications from ProcessWatcher for instant detection.

use smallvec::SmallVec;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info};

use crate::AppState;

pub struct GameSensor {
    state: Arc<AppState>,
}

impl GameSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut process_rx = self.state.process_watcher.subscribe();

        // Publish initial state
        let (game_id, display_name) = self.detect_game().await;
        self.publish_game(&game_id, &display_name).await;

        // Track last published state to avoid duplicate MQTT messages
        let mut last_game_id = game_id;

        info!("Game sensor started (push-based)");

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Game sensor shutting down");
                    break;
                }
                result = process_rx.recv() => {
                    match result {
                        Ok(_notification) => {
                            // Process list changed - re-detect and publish if different
                            let (game_id, display_name) = self.detect_game().await;
                            if game_id != last_game_id {
                                self.publish_game(&game_id, &display_name).await;
                                last_game_id = game_id;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Missed some notifications, just re-detect
                            debug!("Game sensor lagged {} notifications, re-detecting", n);
                            let (game_id, display_name) = self.detect_game().await;
                            if game_id != last_game_id {
                                self.publish_game(&game_id, &display_name).await;
                                last_game_id = game_id;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("Process watcher channel closed");
                            break;
                        }
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

    async fn detect_game(&self) -> (String, String) {
        // Access process list by reference — no HashSet clone (#5)
        let proc_state = self.state.process_watcher.state();
        let proc_guard = proc_state.read().await;

        // Check config games (includes Steam auto-discovered games)
        let config = self.state.config.read().await;

        // Pre-compute lowered patterns once, not per-process (#2)
        let lowered_games: Vec<_> = config
            .games
            .iter()
            .map(|(pattern, gc)| (pattern.to_lowercase(), gc))
            .collect();

        let mut found_games: SmallVec<[(String, String); 4]> = SmallVec::new();
        let mut seen_ids: HashSet<String> = HashSet::with_capacity(lowered_games.len());

        for proc_name in proc_guard.names() {
            let proc_lower = proc_name.to_lowercase();
            let base_name = proc_lower.trim_end_matches(".exe");

            for (pattern_lower, game_config) in &lowered_games {
                if proc_lower.starts_with(pattern_lower.as_str())
                    || base_name == pattern_lower.as_str()
                {
                    let game_id = game_config.game_id().to_string();
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
