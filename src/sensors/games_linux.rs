//! Game detection sensor for Linux - monitors running processes
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)
//!
//! Also publishes a `game_catalog` sensor listing all exposed games from config.

use log::{debug, info, warn};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::{Duration, interval};

/// Latch so a permanently unreadable /proc warns once, not every tick.
static PROC_ENUM_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

    // Long event-loop body: splitting it would scatter tightly-coupled state
    // across helpers for no readability gain. Reviewed, allowed deliberately.
    #[allow(clippy::cognitive_complexity)]
    pub async fn run(self) {
        let config = self.state.config.read().await;
        let mut interval_secs = config.intervals.game_sensor.max(1); // Prevent panic on 0
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
        // Skip only the INITIAL publish when enumeration failed. Returning here
        // would kill the sensor permanently on a single transient /proc read at
        // boot, when the system is busiest, and the supervisor would then respawn
        // it forever on escalating backoff while inflating bridge_health's restart
        // count. The loop below retries on its own.
        let mut last_game_id = match self.detect_game(&cached).await {
            Some(running) => {
                self.publish_game(&running).await;
                running_state(&running).0
            }
            // Nothing published yet; the loop retries and will publish then.
            None => String::new(),
        };

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
                    let new_interval = config.intervals.game_sensor.max(1);
                    let games = config.games.clone();
                    drop(config);
                    // Also pick up a changed poll interval (previously only patterns
                    // were rebuilt, so an interval edit didn't take effect live).
                    if new_interval != interval_secs {
                        interval_secs = new_interval;
                        tick = interval(Duration::from_secs(interval_secs));
                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        debug!("Game sensor: interval changed to {}s", interval_secs);
                    }
                    cached = CachedGamePatterns::build(&games);
                    self.publish_game_catalog(&games).await;
                    debug!("Game sensor: rebuilt cached patterns");
                    let Some(running) = self.detect_game(&cached).await else {
                        continue;
                    };
                    let key = running_state(&running).0;
                    if key != last_game_id {
                        self.publish_game(&running).await;
                        last_game_id = key;
                    }
                }
                // MQTT reconnected - force republish retained state
                r = reconnect_rx.recv() => {
                    // Treat Lagged as a reconnect: the `Ok(())` pattern alone
                    // silently skipped this arm when the 4-slot channel
                    // overran, losing the republish on a flapping broker.
                    // Closed means the sender is gone; `continue` would spin the
                    // loop hot on an instantly-ready recv(). Exit instead.
                    if !matches!(
                        r,
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) {
                        break;
                    }
                    info!("Game sensor: MQTT reconnected, republishing current state");
                    let games = self.state.config.read().await.games.clone();
                    self.publish_game_catalog(&games).await;
                    let Some(running) = self.detect_game(&cached).await else {
                        continue;
                    };
                    self.publish_game(&running).await;
                    last_game_id = running_state(&running).0;
                }
                _ = tick.tick() => {
                    let Some(running) = self.detect_game(&cached).await else {
                        continue;
                    };
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
        self.state.mqtt.publish_sensor("runninggames", &state).await;

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
            .publish_sensor("game_catalog", &count.to_string())
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

    /// `None` when the process table could not be enumerated at all (no `/proc`,
    /// as on macOS, or a restricted one). That is NOT the same as "no game is
    /// running", and collapsing it to an empty Vec here would publish a confident
    /// `runninggames: "none"` forever - defeating the Option that proc_linux
    /// deliberately propagates for exactly this case.
    async fn detect_game(&self, cached: &CachedGamePatterns) -> Option<Vec<(String, String)>> {
        // No configured patterns means nothing can match, so skip the hundreds of
        // /proc reads entirely (a no-games config would otherwise scan every tick).
        if cached.patterns.is_empty() {
            // Genuinely nothing to match - that IS a real empty result.
            return Some(Vec::new());
        }
        // Enumerate processes via /proc
        let processes = match self.get_process_names().await {
            Ok(p) => p,
            Err(e) => {
                // Latched: this is called every tick, so an unreadable /proc
                // (macOS, or a container with /proc masked) otherwise emits an
                // error line per poll - 17k a day at the default interval. Every
                // other persistent-failure path in the repo latches; this one was
                // missed. warn!, not error!: the sensor recovers by itself the
                // moment /proc becomes readable.
                if PROC_ENUM_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    debug!("Failed to enumerate processes: {}", e);
                } else {
                    warn!(
                        "Failed to enumerate processes: {} (further failures silent)",
                        e
                    );
                }
                return None;
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

        Some(found_games)
    }

    async fn get_process_names(&self) -> anyhow::Result<Vec<String>> {
        // /proc enumeration is hundreds of blocking fs::read_to_string calls -
        // run off the single-threaded runtime.
        tokio::task::spawn_blocking(Self::get_process_names_blocking)
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {}", e))?
    }

    fn get_process_names_blocking() -> anyhow::Result<Vec<String>> {
        // Name resolution (incl. the 15-char comm-truncation fallback) is shared
        // with the custom ProcessExists sensor and the close/kill command path.
        // Err, not an empty Vec: an empty list is indistinguishable from "no game
        // is running" and would publish a confident negative forever on a host
        // with no /proc (macOS) or with the table hidden.
        let entries =
            crate::sensors::proc_linux::try_resolve_processes(std::path::Path::new("/proc"))
                .ok_or_else(|| anyhow::anyhow!("cannot enumerate /proc"))?;
        Ok(entries.into_iter().map(|e| e.name).collect())
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
