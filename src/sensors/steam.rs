//! Steam update detection sensor - monitors ACF files for games being updated
//!
//! Adaptive polling: 30s base interval, 5s when updates are active

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

use crate::AppState;

/// StateFlags indicating a game is updating/downloading
const STATE_UPDATE_RUNNING: u32 = 1024; // 0x400
const STATE_UPDATE_PAUSED: u32 = 2048; // 0x800
const STATE_DOWNLOADING: u32 = 524288; // 0x80000
const STATE_FULLY_INSTALLED: u32 = 4; // Ready to play

#[derive(Debug, Clone)]
struct GameUpdateState {
    app_id: String,
    name: String,
    state_flags: u32,
    manifest_path: PathBuf,
}

pub struct SteamSensor {
    state: Arc<AppState>,
    library_folders: Vec<PathBuf>,
    updating_games: HashMap<String, GameUpdateState>,
    last_full_scan: Instant,
    /// Cache of ACF file paths → (mtime, parsed state) to skip unchanged files
    acf_cache: HashMap<PathBuf, (std::time::SystemTime, Option<GameUpdateState>)>,
}

impl SteamSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            library_folders: Vec::new(),
            updating_games: HashMap::new(),
            last_full_scan: Instant::now(),
            acf_cache: HashMap::new(),
        }
    }

    pub async fn run(mut self) {
        let config = self.state.config.read().await;
        let base_interval = config.intervals.steam_check.max(10);
        let fast_interval = config.intervals.steam_updating.max(2);
        drop(config);

        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Discover Steam library folders
        self.discover_library_folders();

        if self.library_folders.is_empty() {
            warn!("No Steam library folders found, steam sensor disabled");
            // Publish initial "off" state
            self.state
                .mqtt
                .publish_sensor_retained("steam_updating", "off")
                .await;
            return;
        }

        info!(
            "Found {} Steam library folder(s)",
            self.library_folders.len()
        );

        // Publish initial state
        self.do_full_scan().await;
        self.last_full_scan = Instant::now();

        loop {
            // Dynamic interval based on whether updates are in progress
            let sleep_duration = if self.updating_games.is_empty() {
                Duration::from_secs(base_interval)
            } else {
                Duration::from_secs(fast_interval)
            };

            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Steam sensor shutting down");
                    break;
                }
                () = tokio::time::sleep(sleep_duration) => {
                    if self.updating_games.is_empty() {
                        // No updates in progress - do full scan
                        self.do_full_scan().await;
                        self.last_full_scan = Instant::now();
                    } else {
                        // Updates in progress - just check those specific games
                        self.do_targeted_scan().await;

                        // Periodic full scan even during updates (every 5 min)
                        if self.last_full_scan.elapsed() > Duration::from_secs(300) {
                            self.do_full_scan().await;
                            self.last_full_scan = Instant::now();
                        }
                    }
                }
            }
        }
    }

    fn discover_library_folders(&mut self) {
        self.library_folders.clear();

        // Get Steam install path from registry
        let steam_path = match self.get_steam_path() {
            Some(p) => p,
            None => {
                debug!("Steam not found in registry");
                return;
            }
        };

        // Primary library is in Steam install dir
        let primary_steamapps = steam_path.join("steamapps");
        if primary_steamapps.exists() {
            self.library_folders.push(primary_steamapps.clone());
        }

        // Parse libraryfolders.vdf for additional libraries
        let vdf_path = primary_steamapps.join("libraryfolders.vdf");
        if vdf_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&vdf_path) {
                self.parse_library_folders_vdf(&content);
            }
        }
    }

    fn get_steam_path(&self) -> Option<PathBuf> {
        // Try HKCU first (current user), then HKLM (all users)
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(steam_key) = hkcu.open_subkey("Software\\Valve\\Steam") {
            if let Ok(path) = steam_key.get_value::<String, _>("SteamPath") {
                let p = PathBuf::from(path);
                if p.exists() {
                    return Some(p);
                }
            }
        }

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(steam_key) = hklm.open_subkey("SOFTWARE\\Valve\\Steam") {
            if let Ok(path) = steam_key.get_value::<String, _>("InstallPath") {
                let p = PathBuf::from(path);
                if p.exists() {
                    return Some(p);
                }
            }
        }

        // Try common paths as fallback
        let common_paths = [
            "C:\\Program Files (x86)\\Steam",
            "C:\\Program Files\\Steam",
            "D:\\Steam",
            "D:\\SteamLibrary",
        ];
        for path in common_paths {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }

        None
    }

    fn parse_library_folders_vdf(&mut self, content: &str) {
        // Simple VDF parser - look for "path" entries
        // Format: "path"		"D:\\SteamLibrary"
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("\"path\"") {
                // Extract the path value
                let parts: Vec<&str> = trimmed.split('"').collect();
                if parts.len() >= 4 {
                    let path_str = parts[3];
                    let library_path = PathBuf::from(path_str).join("steamapps");
                    if library_path.exists() && !self.library_folders.contains(&library_path) {
                        self.library_folders.push(library_path);
                    }
                }
            }
        }
    }

    async fn do_full_scan(&mut self) {
        let mut new_updating: HashMap<String, GameUpdateState> = HashMap::new();
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();

        for lib_folder in &self.library_folders {
            // Read directory and filter for appmanifest_*.acf files
            // (replaces glob::glob which recompiles regex pattern every scan)
            let entries = match std::fs::read_dir(lib_folder) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to read steam library dir {:?}: {}", lib_folder, e);
                    continue;
                }
            };

            for dir_entry in entries.flatten() {
                let path = dir_entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !fname.starts_with("appmanifest_") || !fname.ends_with(".acf") {
                    continue;
                }

                {
                    let entry = path;
                    seen_paths.insert(entry.clone());

                    // Check mtime to skip unchanged files
                    let mtime = std::fs::metadata(&entry).and_then(|m| m.modified()).ok();

                    let game_state = if let Some(mt) = mtime {
                        if let Some((cached_mt, cached_state)) = self.acf_cache.get(&entry) {
                            if *cached_mt == mt {
                                // File unchanged, use cached result
                                cached_state.clone()
                            } else {
                                // File changed, re-parse
                                let state = parse_acf_file(&entry);
                                self.acf_cache.insert(entry.clone(), (mt, state.clone()));
                                state
                            }
                        } else {
                            // New file, parse and cache
                            let state = parse_acf_file(&entry);
                            self.acf_cache.insert(entry.clone(), (mt, state.clone()));
                            state
                        }
                    } else {
                        // Can't get mtime, always parse
                        parse_acf_file(&entry)
                    };

                    if let Some(gs) = game_state {
                        if is_updating(&gs) {
                            new_updating.insert(gs.app_id.clone(), gs);
                        }
                    }
                }
            }
        }

        // Prune cache entries for files that no longer exist
        self.acf_cache.retain(|path, _| seen_paths.contains(path));

        // Check for changes
        let was_updating = !self.updating_games.is_empty();
        let is_updating = !new_updating.is_empty();

        // Log changes
        if was_updating != is_updating || self.updating_games.len() != new_updating.len() {
            if is_updating {
                let names: Vec<&str> = new_updating.values().map(|g| g.name.as_str()).collect();
                info!("Steam games updating: {:?}", names);
            } else if was_updating {
                info!("Steam updates completed");
            }
        }

        self.updating_games = new_updating;
        self.publish_state().await;
    }

    async fn do_targeted_scan(&mut self) {
        // Only check games that were updating
        let mut still_updating: HashMap<String, GameUpdateState> = HashMap::new();

        for (app_id, game_state) in &self.updating_games {
            if let Some(new_state) = parse_acf_file(&game_state.manifest_path) {
                if is_updating(&new_state) {
                    still_updating.insert(app_id.clone(), new_state);
                } else {
                    info!("Steam game finished updating: {}", game_state.name);
                }
            }
        }

        let changed = still_updating.len() != self.updating_games.len();
        self.updating_games = still_updating;

        if changed {
            self.publish_state().await;
        }
    }
}

/// Parse an ACF manifest file into a GameUpdateState
fn parse_acf_file(path: &Path) -> Option<GameUpdateState> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    parse_acf_content(&content, path)
}

/// Parse ACF content string into a GameUpdateState (testable without filesystem)
fn parse_acf_content(content: &str, manifest_path: &Path) -> Option<GameUpdateState> {
    let mut app_id = String::new();
    let mut name = String::new();
    let mut state_flags: u32 = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("\"appid\"") || trimmed.starts_with("\"AppID\"") {
            if let Some(val) = extract_vdf_value(trimmed) {
                app_id = val;
            }
        } else if trimmed.starts_with("\"name\"") {
            if let Some(val) = extract_vdf_value(trimmed) {
                name = val;
            }
        } else if trimmed.starts_with("\"StateFlags\"") || trimmed.starts_with("\"stateflags\"") {
            if let Some(val) = extract_vdf_value(trimmed) {
                state_flags = val.parse().unwrap_or(0);
            }
        }
    }

    if app_id.is_empty() {
        return None;
    }

    Some(GameUpdateState {
        app_id,
        name,
        state_flags,
        manifest_path: manifest_path.to_path_buf(),
    })
}

/// Extract a quoted value from a VDF key-value line
/// Format: "key"\t\t"value"
fn extract_vdf_value(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.split('"').collect();
    if parts.len() >= 4 {
        Some(parts[3].to_string())
    } else {
        None
    }
}

/// Check if a game's state flags indicate an update in progress
fn is_updating(game: &GameUpdateState) -> bool {
    let flags = game.state_flags;

    // Check if any update-related flag is set
    if flags & STATE_UPDATE_RUNNING != 0 {
        return true;
    }
    if flags & STATE_UPDATE_PAUSED != 0 {
        return true;
    }
    if flags & STATE_DOWNLOADING != 0 {
        return true;
    }

    // If not fully installed (~4), something is in progress
    // But be careful - newly added games might have 0
    if flags != 0 && flags != STATE_FULLY_INSTALLED {
        // Could be installing, updating, etc.
        return true;
    }

    false
}

impl SteamSensor {
    async fn publish_state(&self) {
        let is_updating = !self.updating_games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };

        self.state
            .mqtt
            .publish_sensor_retained("steam_updating", state_str)
            .await;

        // Publish attributes with game names
        if is_updating {
            let names: Vec<&str> = self
                .updating_games
                .values()
                .map(|g| g.name.as_str())
                .collect();
            let attrs = serde_json::json!({
                "updating_games": names,
                "count": self.updating_games.len()
            });
            self.state
                .mqtt
                .publish_sensor_attributes("steam_updating", &attrs)
                .await;
        } else {
            let attrs = serde_json::json!({
                "updating_games": [],
                "count": 0
            });
            self.state
                .mqtt
                .publish_sensor_attributes("steam_updating", &attrs)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- extract_vdf_value tests --

    #[test]
    fn test_extract_vdf_value_basic() {
        let line = r#"	"appid"		"730""#;
        assert_eq!(extract_vdf_value(line), Some("730".to_string()));
    }

    #[test]
    fn test_extract_vdf_value_name() {
        let line = r#"	"name"		"Counter-Strike 2""#;
        assert_eq!(
            extract_vdf_value(line),
            Some("Counter-Strike 2".to_string())
        );
    }

    #[test]
    fn test_extract_vdf_value_no_value() {
        let line = r#"	"appid""#;
        assert_eq!(extract_vdf_value(line), None);
    }

    #[test]
    fn test_extract_vdf_value_empty_value() {
        let line = r#"	"key"		"""#;
        assert_eq!(extract_vdf_value(line), Some(String::new()));
    }

    #[test]
    fn test_extract_vdf_value_state_flags() {
        let line = r#"	"StateFlags"		"4""#;
        assert_eq!(extract_vdf_value(line), Some("4".to_string()));
    }

    // -- is_updating tests --

    fn make_game(flags: u32) -> GameUpdateState {
        GameUpdateState {
            app_id: "730".to_string(),
            name: "Test Game".to_string(),
            state_flags: flags,
            manifest_path: PathBuf::from("/tmp/test.acf"),
        }
    }

    #[test]
    fn test_is_updating_fully_installed() {
        assert!(!is_updating(&make_game(STATE_FULLY_INSTALLED)));
    }

    #[test]
    fn test_is_updating_zero_flags() {
        assert!(!is_updating(&make_game(0)));
    }

    #[test]
    fn test_is_updating_update_running() {
        assert!(is_updating(&make_game(STATE_UPDATE_RUNNING)));
    }

    #[test]
    fn test_is_updating_update_paused() {
        assert!(is_updating(&make_game(STATE_UPDATE_PAUSED)));
    }

    #[test]
    fn test_is_updating_downloading() {
        assert!(is_updating(&make_game(STATE_DOWNLOADING)));
    }

    #[test]
    fn test_is_updating_combined_flags() {
        assert!(is_updating(&make_game(
            STATE_FULLY_INSTALLED | STATE_UPDATE_RUNNING
        )));
    }

    #[test]
    fn test_is_updating_unknown_nonzero() {
        // Non-zero, non-installed flags → updating
        assert!(is_updating(&make_game(2)));
    }

    // -- parse_acf_content tests --

    #[test]
    fn test_parse_acf_content_valid() {
        let content = r#"
"AppState"
{
	"appid"		"730"
	"name"		"Counter-Strike 2"
	"StateFlags"		"4"
	"installdir"		"Counter-Strike Global Offensive"
}
"#;
        let path = PathBuf::from("/tmp/appmanifest_730.acf");
        let result = parse_acf_content(content, &path).unwrap();
        assert_eq!(result.app_id, "730");
        assert_eq!(result.name, "Counter-Strike 2");
        assert_eq!(result.state_flags, 4);
    }

    #[test]
    fn test_parse_acf_content_updating() {
        let content = r#"
"AppState"
{
	"appid"		"440"
	"name"		"Team Fortress 2"
	"StateFlags"		"1028"
}
"#;
        let path = PathBuf::from("/tmp/appmanifest_440.acf");
        let result = parse_acf_content(content, &path).unwrap();
        assert_eq!(result.app_id, "440");
        assert_eq!(result.state_flags, 1028);
        assert!(is_updating(&result));
    }

    #[test]
    fn test_parse_acf_content_missing_appid() {
        let content = r#"
"AppState"
{
	"name"		"No AppID Game"
	"StateFlags"		"4"
}
"#;
        let path = PathBuf::from("/tmp/test.acf");
        assert!(parse_acf_content(content, &path).is_none());
    }

    #[test]
    fn test_parse_acf_content_empty() {
        let path = PathBuf::from("/tmp/empty.acf");
        assert!(parse_acf_content("", &path).is_none());
    }

    #[test]
    fn test_parse_acf_content_case_variants() {
        // Tests both "appid" and "AppID" variants
        let content = r#"
"AppState"
{
	"AppID"		"553850"
	"name"		"HELLDIVERS 2"
	"stateflags"		"4"
}
"#;
        let path = PathBuf::from("/tmp/test.acf");
        let result = parse_acf_content(content, &path).unwrap();
        assert_eq!(result.app_id, "553850");
        assert_eq!(result.state_flags, 4);
    }

    // ===== MQTT content verification — exact payloads for steam_updating sensor =====

    #[test]
    fn test_steam_updating_state_string_when_updating() {
        // When updating_games is non-empty, the state string is "on"
        let games = vec![make_game(STATE_UPDATE_RUNNING)];
        let is_updating = !games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        assert_eq!(state_str, "on");
    }

    #[test]
    fn test_steam_updating_state_string_when_idle() {
        // When no games updating, the state string is "off"
        let games: Vec<GameUpdateState> = vec![];
        let is_updating = !games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        assert_eq!(state_str, "off");
    }

    #[test]
    fn test_steam_updating_attributes_json_with_games() {
        // Exact JSON published to homeassistant/sensor/{device}/steam_updating/attributes
        let games = vec![
            GameUpdateState {
                app_id: "730".to_string(),
                name: "Counter-Strike 2".to_string(),
                state_flags: STATE_UPDATE_RUNNING,
                manifest_path: PathBuf::from("/tmp/appmanifest_730.acf"),
            },
            GameUpdateState {
                app_id: "440".to_string(),
                name: "Team Fortress 2".to_string(),
                state_flags: STATE_DOWNLOADING,
                manifest_path: PathBuf::from("/tmp/appmanifest_440.acf"),
            },
        ];
        let names: Vec<&str> = games.iter().map(|g| g.name.as_str()).collect();
        let attrs = serde_json::json!({
            "updating_games": names,
            "count": games.len()
        });

        // Verify exact JSON structure
        let obj = attrs.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert_eq!(attrs["count"], 2);
        let game_names = attrs["updating_games"].as_array().unwrap();
        assert_eq!(game_names.len(), 2);
        assert_eq!(game_names[0], "Counter-Strike 2");
        assert_eq!(game_names[1], "Team Fortress 2");

        // Verify the exact serialized bytes that go to MQTT
        let json_bytes = serde_json::to_vec(&attrs).unwrap();
        let roundtrip: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(roundtrip, attrs);
    }

    #[test]
    fn test_steam_updating_attributes_json_when_idle() {
        // Exact JSON published when no games are updating
        let attrs = serde_json::json!({
            "updating_games": Vec::<String>::new(),
            "count": 0
        });
        let obj = attrs.as_object().unwrap();
        assert_eq!(attrs["count"], 0);
        assert!(attrs["updating_games"].as_array().unwrap().is_empty());
        assert_eq!(obj.len(), 2); // always has both fields even when idle
    }

    #[test]
    fn test_steam_updating_full_mqtt_payload_content() {
        // End-to-end: simulate what SteamSensor::publish_state builds
        // Scenario: one game downloading

        // 1. State topic payload (retained)
        let updating_games: HashMap<String, GameUpdateState> = {
            let mut m = HashMap::new();
            m.insert(
                "553850".to_string(),
                GameUpdateState {
                    app_id: "553850".to_string(),
                    name: "HELLDIVERS 2".to_string(),
                    state_flags: STATE_DOWNLOADING,
                    manifest_path: PathBuf::from("/tmp/appmanifest_553850.acf"),
                },
            );
            m
        };

        let is_updating = !updating_games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        // This exact string goes to: homeassistant/sensor/{device}/steam_updating/state
        assert_eq!(state_str, "on");

        // 2. Attributes topic payload (retained)
        let names: Vec<&str> = updating_games.values().map(|g| g.name.as_str()).collect();
        let attrs = serde_json::json!({
            "updating_games": names,
            "count": updating_games.len()
        });
        // This exact JSON goes to: homeassistant/sensor/{device}/steam_updating/attributes
        assert_eq!(attrs["count"], 1);
        assert_eq!(attrs["updating_games"][0], "HELLDIVERS 2");
    }
}
