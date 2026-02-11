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

/// Cached lowered game patterns to avoid recomputing on every WMI event
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
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut process_rx = self.state.process_watcher.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // Build cached patterns once at startup
        let config = self.state.config.read().await;
        let mut cached = CachedGamePatterns::build(&config.games);
        drop(config);

        // Publish initial state
        let (game_id, display_name) = self.detect_game(&cached).await;
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
                // Rebuild cached patterns when config changes
                Ok(()) = config_rx.recv() => {
                    let config = self.state.config.read().await;
                    cached = CachedGamePatterns::build(&config.games);
                    drop(config);
                    debug!("Game sensor: rebuilt cached patterns");
                    // Re-detect with new patterns
                    let (game_id, display_name) = self.detect_game(&cached).await;
                    if game_id != last_game_id {
                        self.publish_game(&game_id, &display_name).await;
                        last_game_id = game_id;
                    }
                }
                result = process_rx.recv() => {
                    match result {
                        Ok(_notification) => {
                            // Process list changed - re-detect and publish if different
                            let (game_id, display_name) = self.detect_game(&cached).await;
                            if game_id != last_game_id {
                                self.publish_game(&game_id, &display_name).await;
                                last_game_id = game_id;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Missed some notifications, just re-detect
                            debug!("Game sensor lagged {} notifications, re-detecting", n);
                            let (game_id, display_name) = self.detect_game(&cached).await;
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

    async fn detect_game(&self, cached: &CachedGamePatterns) -> (String, String) {
        // Access process list by reference — no HashSet clone
        let proc_state = self.state.process_watcher.state();
        let proc_guard = proc_state.read().await;

        let mut found_games: SmallVec<[(String, String); 4]> = SmallVec::new();
        let mut seen_ids: HashSet<&str> = HashSet::with_capacity(cached.patterns.len());

        for proc_name in proc_guard.names() {
            let proc_lower = proc_name.to_lowercase();
            let base_name = proc_lower.trim_end_matches(".exe");

            for (pattern_lower, game_id, display_name) in &cached.patterns {
                if (proc_lower.starts_with(pattern_lower.as_str())
                    || base_name == pattern_lower.as_str())
                    && seen_ids.insert(game_id.as_str())
                {
                    found_games.push((game_id.clone(), display_name.clone()));
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
