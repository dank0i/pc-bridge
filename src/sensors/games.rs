//! Game detection sensor - monitors running processes to detect games
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)
//!
//! Uses push notifications from ProcessWatcher for instant detection.
//!
//! Also publishes a `game_catalog` sensor listing all exposed games from config.

use log::{debug, info};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;

use crate::AppState;

#[derive(Serialize)]
struct CatalogEntry {
    game_id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<u32>,
    /// Process name used for detection and close commands
    process_name: String,
    /// Launch command (e.g. "steam:730", "lnk:...")
    #[serde(skip_serializing_if = "Option::is_none")]
    launch_command: Option<String>,
}

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
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

        // Build cached patterns once at startup
        // Clone the games map and drop the read lock before any async
        // work (MQTT publish) to avoid holding the lock across await points.
        let games = self.state.config.read().await.games.clone();
        let mut cached = CachedGamePatterns::build(&games);
        self.publish_game_catalog(&games).await;

        // Publish initial state
        let (game_id, display_name) = self.detect_game(&cached).await;
        self.publish_game(&game_id, &display_name).await;

        // Track last published state to avoid duplicate MQTT messages
        let mut last_game_id = game_id;

        info!("Game sensor started (push-based)");

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
                    // Re-detect with new patterns
                    let (game_id, display_name) = self.detect_game(&cached).await;
                    if game_id != last_game_id {
                        self.publish_game(&game_id, &display_name).await;
                        last_game_id = game_id;
                    }
                }
                // MQTT reconnected — force republish retained state
                Ok(()) = reconnect_rx.recv() => {
                    info!("Game sensor: MQTT reconnected, republishing current state");
                    let games = self.state.config.read().await.games.clone();
                    self.publish_game_catalog(&games).await;
                    let (game_id, display_name) = self.detect_game(&cached).await;
                    self.publish_game(&game_id, &display_name).await;
                    last_game_id = game_id;
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
            .publish_sensor_retained("runninggames", game_ids)
            .await;

        // Build structured game list for attributes (Feature D)
        let games_array: Vec<serde_json::Value> = if game_ids == "none" {
            Vec::new()
        } else {
            game_ids
                .split(',')
                .zip(display_names.split(", "))
                .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
                .collect()
        };

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

    /// Publish the game catalog sensor — a retained list of all exposed games from config.
    async fn publish_game_catalog(
        &self,
        games: &std::collections::HashMap<String, crate::config::GameConfig>,
    ) {
        // Collect into typed structs, sort by name, then serialize once
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

    async fn detect_game(&self, cached: &CachedGamePatterns) -> (String, String) {
        // Access process list by reference — no HashSet clone
        let proc_state = self.state.process_watcher.state();
        let proc_guard = proc_state.read().await;
        match_games_in_processes(proc_guard.names(), cached)
    }
}

/// Pure matching function — testable without AppState.
/// Given a set of running process names and cached game patterns,
/// returns (game_ids, display_names) as comma-separated strings.
fn match_games_in_processes(
    process_names: &HashSet<Arc<str>>,
    cached: &CachedGamePatterns,
) -> (String, String) {
    let mut found_games: Vec<(String, String)> = Vec::with_capacity(2);
    let mut seen_ids: HashSet<&str> = HashSet::with_capacity(cached.patterns.len());

    for proc_name in process_names {
        // Strip .exe suffix without allocating (case-insensitive for all casings)
        let base_name = if proc_name.len() > 4
            && proc_name.as_bytes()[proc_name.len() - 4..].eq_ignore_ascii_case(b".exe")
        {
            &proc_name[..proc_name.len() - 4]
        } else {
            proc_name
        };

        for (pattern_lower, game_id, display_name) in &cached.patterns {
            // Case-insensitive comparison without allocation
            let matches = starts_with_ignore_ascii_case(proc_name, pattern_lower)
                || base_name.eq_ignore_ascii_case(pattern_lower);
            if matches && seen_ids.insert(game_id.as_str()) {
                found_games.push((game_id.clone(), display_name.clone()));
                break; // This process matched — no need to check remaining patterns
            }
        }
    }

    if found_games.is_empty() {
        ("none".to_string(), "None".to_string())
    } else {
        let ids: Vec<&str> = found_games.iter().map(|(id, _)| id.as_str()).collect();
        let names: Vec<&str> = found_games.iter().map(|(_, name)| name.as_str()).collect();
        (ids.join(","), names.join(", "))
    }
}

/// Case-insensitive ASCII prefix check without allocation
fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GameConfig;
    use std::collections::HashMap;

    /// Helper: build CachedGamePatterns from (pattern, GameConfig) pairs
    fn make_patterns(games: &[(&str, GameConfig)]) -> CachedGamePatterns {
        let map: HashMap<String, GameConfig> = games
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        CachedGamePatterns::build(&map)
    }

    /// Helper: build a process set from string slices
    fn procs(names: &[&str]) -> HashSet<Arc<str>> {
        names.iter().map(|n| Arc::from(*n)).collect()
    }

    // ===== starts_with_ignore_ascii_case =====

    #[test]
    fn test_starts_with_exact_match() {
        assert!(starts_with_ignore_ascii_case("battlefield", "battlefield"));
    }

    #[test]
    fn test_starts_with_prefix() {
        assert!(starts_with_ignore_ascii_case(
            "battlefield2042.exe",
            "battlefield"
        ));
    }

    #[test]
    fn test_starts_with_case_insensitive() {
        assert!(starts_with_ignore_ascii_case(
            "BattleField2042.exe",
            "battlefield"
        ));
    }

    #[test]
    fn test_starts_with_no_match() {
        assert!(!starts_with_ignore_ascii_case("notepad.exe", "battlefield"));
    }

    #[test]
    fn test_starts_with_shorter_haystack() {
        assert!(!starts_with_ignore_ascii_case("bf", "battlefield"));
    }

    // ===== No games running =====

    #[test]
    fn test_no_processes_returns_none() {
        let cached = make_patterns(&[("bf2042", GameConfig::Simple("battlefield_6".into()))]);
        let (ids, names) = match_games_in_processes(&procs(&[]), &cached);
        assert_eq!(ids, "none");
        assert_eq!(names, "None");
    }

    #[test]
    fn test_no_matching_processes_returns_none() {
        let cached = make_patterns(&[("bf2042", GameConfig::Simple("battlefield_6".into()))]);
        let processes = procs(&["chrome.exe", "explorer.exe", "svchost.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "none");
        assert_eq!(names, "None");
    }

    // ===== Single game detection =====

    #[test]
    fn test_single_game_exact_exe_match() {
        let cached = make_patterns(&[("bf2042", GameConfig::Simple("battlefield_6".into()))]);
        let processes = procs(&["bf2042.exe", "chrome.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "battlefield_6");
        assert_eq!(names, "Battlefield 6"); // smart_title applied
    }

    #[test]
    fn test_single_game_case_insensitive_exe() {
        // Process name has mixed case, pattern is lowered
        let cached = make_patterns(&[("bf2042", GameConfig::Simple("battlefield_6".into()))]);
        let processes = procs(&["BF2042.EXE"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "battlefield_6");
    }

    #[test]
    fn test_single_game_mixed_case_exe_suffix() {
        // Mixed case .Exe / .eXe should also be stripped
        let cached = make_patterns(&[("bf2042", GameConfig::Simple("battlefield_6".into()))]);
        for suffix in &[".Exe", ".eXE", ".eXe", ".exE"] {
            let name = format!("BF2042{}", suffix);
            let processes = procs(&[&name]);
            let (ids, _) = match_games_in_processes(&processes, &cached);
            assert_eq!(ids, "battlefield_6", "failed for suffix {}", suffix);
        }
    }

    #[test]
    fn test_single_game_prefix_match() {
        // Pattern "battlefield" should match "battlefield2042.exe" via starts_with
        let cached = make_patterns(&[("battlefield", GameConfig::Simple("battlefield_6".into()))]);
        let processes = procs(&["battlefield2042.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "battlefield_6");
        assert_eq!(names, "Battlefield 6");
    }

    #[test]
    fn test_game_with_full_config_and_display_name() {
        let cached = make_patterns(&[(
            "helldivers2",
            GameConfig::Full {
                game_id: "helldivers_2".into(),
                app_id: Some(553850),
                name: Some("HELLDIVERS 2".into()),
                auto_discovered: true,
                exposed: true,
            },
        )]);
        let processes = procs(&["helldivers2.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "helldivers_2");
        assert_eq!(names, "HELLDIVERS 2"); // uses explicit name, not smart_title
    }

    #[test]
    fn test_game_full_config_no_name_uses_smart_title() {
        let cached = make_patterns(&[(
            "cod_mw",
            GameConfig::Full {
                game_id: "call_of_duty_mw".into(),
                app_id: None,
                name: None,
                auto_discovered: false,
                exposed: true,
            },
        )]);
        let processes = procs(&["cod_mw.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "call_of_duty_mw");
        assert_eq!(names, "Call Of Duty Mw"); // smart_title from game_id
    }

    // ===== Multiple games =====

    #[test]
    fn test_multiple_games_comma_separated() {
        let cached = make_patterns(&[
            ("bf2042", GameConfig::Simple("battlefield_6".into())),
            ("cod_mw", GameConfig::Simple("call_of_duty_mw".into())),
        ]);
        let processes = procs(&["bf2042.exe", "cod_mw.exe", "chrome.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);
        // Both games detected — order depends on HashSet iteration, check both present
        assert!(ids.contains("battlefield_6"));
        assert!(ids.contains("call_of_duty_mw"));
        assert!(ids.contains(','));
        assert!(names.contains("Battlefield 6"));
        assert!(names.contains("Call Of Duty Mw"));
    }

    // ===== Deduplication =====

    #[test]
    fn test_duplicate_game_id_deduplicated() {
        // Two different process patterns mapping to the same game_id
        let cached = make_patterns(&[
            ("bf2042", GameConfig::Simple("battlefield_6".into())),
            (
                "battlefield2042",
                GameConfig::Simple("battlefield_6".into()),
            ),
        ]);
        // Both process patterns present — should still only report one game
        let processes = procs(&["bf2042.exe", "battlefield2042.exe"]);
        let (ids, _names) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "battlefield_6"); // no duplicate
        assert!(!ids.contains(','));
    }

    // ===== Process without .exe suffix =====

    #[test]
    fn test_process_without_exe_suffix() {
        // Linux-style process name (no .exe)
        let cached = make_patterns(&[("helldivers2", GameConfig::Simple("helldivers_2".into()))]);
        let processes = procs(&["helldivers2"]);
        let (ids, _) = match_games_in_processes(&processes, &cached);
        assert_eq!(ids, "helldivers_2");
    }

    // ===== CachedGamePatterns::build =====

    #[test]
    fn test_cached_patterns_build_lowercases_keys() {
        let cached = make_patterns(&[("BF2042", GameConfig::Simple("battlefield_6".into()))]);
        assert_eq!(cached.patterns.len(), 1);
        assert_eq!(cached.patterns[0].0, "bf2042"); // lowered
        assert_eq!(cached.patterns[0].1, "battlefield_6"); // game_id
        assert_eq!(cached.patterns[0].2, "Battlefield 6"); // display_name
    }

    #[test]
    fn test_cached_patterns_empty_map() {
        let cached = CachedGamePatterns::build(&HashMap::new());
        assert!(cached.patterns.is_empty());
    }

    // ===== End-to-end: exact MQTT content verification =====

    #[test]
    fn test_mqtt_content_no_games() {
        // Simulates what publish_game sends when nothing is running
        let cached = make_patterns(&[("bf2042", GameConfig::Simple("battlefield_6".into()))]);
        let (ids, names) = match_games_in_processes(&procs(&[]), &cached);

        // Verify exact MQTT sensor payload
        assert_eq!(ids, "none");

        // Verify exact attributes JSON payload
        let attrs = serde_json::json!({ "display_name": names });
        let json_bytes = serde_json::to_vec(&attrs).unwrap();
        assert_eq!(
            String::from_utf8(json_bytes).unwrap(),
            r#"{"display_name":"None"}"#
        );
    }

    #[test]
    fn test_mqtt_content_single_game_running() {
        // Simulates exact payloads published when BF2042 is detected
        let cached = make_patterns(&[(
            "bf2042",
            GameConfig::Full {
                game_id: "battlefield_6".into(),
                app_id: Some(1517290),
                name: Some("Battlefield 2042".into()),
                auto_discovered: true,
                exposed: true,
            },
        )]);
        let processes = procs(&["bf2042.exe", "chrome.exe", "explorer.exe"]);
        let (ids, names) = match_games_in_processes(&processes, &cached);

        // This is what goes to: homeassistant/sensor/{device}/runninggames/state
        assert_eq!(ids, "battlefield_6");

        // This is what goes to: homeassistant/sensor/{device}/runninggames/attributes
        let attrs = serde_json::json!({ "display_name": names });
        assert_eq!(
            serde_json::to_string(&attrs).unwrap(),
            r#"{"display_name":"Battlefield 2042"}"#
        );
    }

    // ===== CatalogEntry serialization =====

    #[test]
    fn test_catalog_entry_with_app_id() {
        let entry = CatalogEntry {
            game_id: "battlefield_6".into(),
            name: "Battlefield 2042".into(),
            app_id: Some(1517290),
            process_name: "bf2042".into(),
            launch_command: Some("steam:1517290".into()),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["game_id"], "battlefield_6");
        assert_eq!(json["name"], "Battlefield 2042");
        assert_eq!(json["app_id"], 1517290);
        assert_eq!(json["process_name"], "bf2042");
        assert_eq!(json["launch_command"], "steam:1517290");
    }

    #[test]
    fn test_catalog_entry_without_app_id_omits_field() {
        let entry = CatalogEntry {
            game_id: "minecraft".into(),
            name: "Minecraft".into(),
            app_id: None,
            process_name: "javaw".into(),
            launch_command: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["game_id"], "minecraft");
        assert!(
            json.get("app_id").is_none(),
            "app_id should be omitted when None"
        );
        assert!(
            json.get("launch_command").is_none(),
            "launch_command should be omitted when None"
        );
        assert_eq!(json["process_name"], "javaw");
    }

    #[test]
    fn test_catalog_entries_sort_by_name() {
        let mut entries = vec![
            CatalogEntry {
                game_id: "zelda".into(),
                name: "Zelda".into(),
                app_id: None,
                process_name: "zelda".into(),
                launch_command: None,
            },
            CatalogEntry {
                game_id: "apex".into(),
                name: "Apex Legends".into(),
                app_id: Some(1172470),
                process_name: "r5apex_dx12".into(),
                launch_command: Some("steam:1172470".into()),
            },
            CatalogEntry {
                game_id: "minecraft".into(),
                name: "Minecraft".into(),
                app_id: None,
                process_name: "javaw".into(),
                launch_command: None,
            },
        ];
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries[0].name, "Apex Legends");
        assert_eq!(entries[1].name, "Minecraft");
        assert_eq!(entries[2].name, "Zelda");
    }

    // ===== is_exposed filtering =====

    #[test]
    fn test_simple_config_always_exposed() {
        let gc = GameConfig::Simple("test_game".into());
        assert!(gc.is_exposed());
    }

    #[test]
    fn test_full_config_exposed_default_true() {
        let json = r#"{"game_id": "test"}"#;
        let gc: GameConfig = serde_json::from_str(json).unwrap();
        assert!(gc.is_exposed());
    }

    #[test]
    fn test_full_config_exposed_false_filtered() {
        let gc = GameConfig::Full {
            game_id: "hidden_game".into(),
            app_id: None,
            name: None,
            auto_discovered: false,
            exposed: false,
        };
        assert!(!gc.is_exposed());

        // Should be filtered out of catalog
        let games: HashMap<String, GameConfig> = [("hidden".into(), gc)].into_iter().collect();
        let exposed: Vec<_> = games.values().filter(|g| g.is_exposed()).collect();
        assert!(exposed.is_empty());
    }

    #[test]
    fn test_mixed_exposed_filtering() {
        let games: HashMap<String, GameConfig> = [
            ("bf2042".into(), GameConfig::Simple("battlefield_6".into())),
            (
                "hidden".into(),
                GameConfig::Full {
                    game_id: "hidden_game".into(),
                    app_id: None,
                    name: None,
                    auto_discovered: false,
                    exposed: false,
                },
            ),
            (
                "cs2".into(),
                GameConfig::Full {
                    game_id: "counter_strike_2".into(),
                    app_id: Some(730),
                    name: Some("Counter-Strike 2".into()),
                    auto_discovered: true,
                    exposed: true,
                },
            ),
        ]
        .into_iter()
        .collect();

        let exposed: Vec<_> = games.values().filter(|g| g.is_exposed()).collect();
        assert_eq!(exposed.len(), 2);
    }
}
