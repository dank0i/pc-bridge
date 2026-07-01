//! Game detection sensor for Linux - monitors running processes
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)
//!
//! Also publishes a `game_catalog` sensor listing all exposed games from config.

use log::{debug, error, info};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use tokio::time::{Duration, interval};

use crate::AppState;

#[derive(Serialize)]
struct CatalogEntry {
    game_id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<u32>,
    process_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    launch_command: Option<String>,
}

/// Cached lowered game patterns - rebuilt only when config changes via config_generation.
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
        let games = config.games.clone();
        let mut cached = CachedGamePatterns::build(&games);
        drop(config);
        self.publish_game_catalog(&games).await;

        let mut tick = interval(Duration::from_secs(interval_secs));
        // Skip missed ticks so a suspend/resume doesn't fire a burst of catch-up
        // game scans.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

        // Publish initial state
        let running = self.detect_game(&cached).await;
        self.publish_game(&running).await;

        // Track last published state (joined ids) to avoid duplicate messages
        let mut last_game_id = running_state(&running).0;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Game sensor shutting down");
                    break;
                }
                // Rebuild cached patterns when config changes
                Ok(()) = config_rx.recv() => {
                    let games = self.state.config.read().await.games.clone();
                    cached = CachedGamePatterns::build(&games);
                    self.publish_game_catalog(&games).await;
                    debug!("Game sensor: rebuilt cached patterns");
                    let running = self.detect_game(&cached).await;
                    let key = running_state(&running).0;
                    if key != last_game_id {
                        self.publish_game(&running).await;
                        last_game_id = key;
                    }
                }
                // MQTT reconnected - force republish retained state
                Ok(()) = reconnect_rx.recv() => {
                    info!("Game sensor: MQTT reconnected, republishing current state");
                    let games = self.state.config.read().await.games.clone();
                    self.publish_game_catalog(&games).await;
                    let running = self.detect_game(&cached).await;
                    self.publish_game(&running).await;
                    last_game_id = running_state(&running).0;
                }
                _ = tick.tick() => {
                    let running = self.detect_game(&cached).await;
                    let key = running_state(&running).0;
                    if key != last_game_id {
                        self.publish_game(&running).await;
                        last_game_id = key;
                    }
                }
            }
        }
    }

    async fn publish_game(&self, games: &[(String, String)]) {
        let (state, display_names) = running_state(games);
        self.state
            .mqtt
            .publish_sensor_retained("runninggames", &state)
            .await;

        // Structured game list, built directly from the pairs (no re-split).
        let games_array: Vec<serde_json::Value> = games
            .iter()
            .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
            .collect();

        let attrs = serde_json::json!({
            "display_name": display_names,
            "games": games_array,
            "count": games_array.len(),
        });
        self.state
            .mqtt
            .publish_sensor_attributes("runninggames", &attrs)
            .await;
    }

    /// Publish the game catalog sensor - a retained list of all exposed games from config.
    async fn publish_game_catalog(
        &self,
        games: &std::collections::HashMap<String, crate::config::GameConfig>,
    ) {
        let mut entries: Vec<CatalogEntry> = games
            .iter()
            .filter(|(_, gc)| gc.is_exposed())
            .map(|(process_pattern, gc)| CatalogEntry {
                game_id: gc.game_id().to_owned(),
                name: gc.display_name(),
                app_id: gc.app_id(),
                process_name: process_pattern.clone(),
                launch_command: gc.launch_command(),
            })
            .collect();

        entries.sort_by(|a, b| a.name.cmp(&b.name));

        let count = entries.len();
        self.state
            .mqtt
            .publish_sensor_retained("game_catalog", &count.to_string())
            .await;
        let attrs = serde_json::json!({
            "games": entries,
            "count": count,
        });
        self.state
            .mqtt
            .publish_sensor_attributes("game_catalog", &attrs)
            .await;
        debug!("Published game catalog with {} exposed games", count);
    }

    async fn detect_game(&self, cached: &CachedGamePatterns) -> Vec<(String, String)> {
        // Enumerate processes via /proc
        let processes = match self.get_process_names().await {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to enumerate processes: {}", e);
                return Vec::new();
            }
        };

        let mut found_games: Vec<(String, String)> = Vec::with_capacity(2);
        let mut seen_ids: HashSet<&str> = HashSet::with_capacity(cached.patterns.len());

        for proc_name in &processes {
            for (pattern_lower, game_id, display_name) in &cached.patterns {
                // Case-insensitive prefix match OR exact match (matches Windows behavior)
                let matches = starts_with_ignore_ascii_case(proc_name, pattern_lower)
                    || proc_name.eq_ignore_ascii_case(pattern_lower);
                if matches && seen_ids.insert(game_id.as_str()) {
                    found_games.push((game_id.clone(), display_name.clone()));
                    break; // This process matched - no need to check remaining patterns
                }
            }
        }

        found_games
    }

    async fn get_process_names(&self) -> anyhow::Result<Vec<String>> {
        // /proc enumeration is hundreds of blocking fs::read_to_string calls -
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
                // /proc/<pid>/comm is truncated to 15 bytes (TASK_COMM_LEN-1), so a
                // game with a longer executable name (e.g. MarvelRivals_Shipping)
                // would never match. When comm is at the truncation length, prefer
                // the untruncated basename from cmdline.
                let comm = fs::read_to_string(path.join("comm"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let name = if comm.len() >= 15 {
                    fs::read_to_string(path.join("cmdline"))
                        .ok()
                        .and_then(|cl| {
                            cl.split('\0')
                                .next()
                                .filter(|a| !a.is_empty())
                                .map(|a0| a0.rsplit(['/', '\\']).next().unwrap_or(a0).to_string())
                        })
                        .filter(|b| !b.is_empty())
                        .unwrap_or(comm)
                } else {
                    comm
                };
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }

        Ok(names)
    }
}

/// Case-insensitive ASCII prefix check without allocation. An empty prefix never
/// matches - otherwise a blank/misconfigured game pattern reports every process.
fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    !prefix.is_empty()
        && haystack.len() >= prefix.len()
        && haystack.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Retained sensor value (comma-joined ids) and display string (", "-joined
/// names) for the running games. "none"/"None" when empty.
fn running_state(games: &[(String, String)]) -> (String, String) {
    if games.is_empty() {
        return ("none".to_string(), "None".to_string());
    }
    let ids: Vec<&str> = games.iter().map(|(id, _)| id.as_str()).collect();
    let names: Vec<&str> = games.iter().map(|(_, name)| name.as_str()).collect();
    (ids.join(","), names.join(", "))
}

/// Read currently-running process names from `/proc` (blocking). Exposed for the
/// `CloseGame` command, which has no process watcher on Linux.
pub(crate) fn current_process_names() -> Vec<String> {
    GameSensor::get_process_names_blocking().unwrap_or_default()
}
