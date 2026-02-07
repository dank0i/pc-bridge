//! Steam update detection sensor - monitors ACF files for games being updated
//!
//! Adaptive polling: 30s base interval, 5s when updates are active

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{interval, Duration, Instant};
use tracing::{debug, error, info, warn};
use winreg::enums::*;
use winreg::RegKey;

use crate::AppState;

/// StateFlags indicating a game is updating/downloading
const STATE_UPDATE_RUNNING: u32 = 1024;      // 0x400
const STATE_UPDATE_PAUSED: u32 = 2048;       // 0x800
const STATE_DOWNLOADING: u32 = 524288;       // 0x80000
const STATE_FULLY_INSTALLED: u32 = 4;        // Ready to play

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
}

impl SteamSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            library_folders: Vec::new(),
            updating_games: HashMap::new(),
            last_full_scan: Instant::now(),
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
            self.state.mqtt.publish_sensor_retained("steam_updating", "off").await;
            return;
        }

        info!("Found {} Steam library folder(s)", self.library_folders.len());

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
                _ = shutdown_rx.recv() => {
                    debug!("Steam sensor shutting down");
                    break;
                }
                _ = tokio::time::sleep(sleep_duration) => {
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

        for lib_folder in &self.library_folders {
            // Glob for appmanifest_*.acf files
            let pattern = lib_folder.join("appmanifest_*.acf");
            if let Some(pattern_str) = pattern.to_str() {
                match glob::glob(pattern_str) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            if let Some(game_state) = self.parse_acf_file(&entry) {
                                if self.is_updating(&game_state) {
                                    new_updating.insert(game_state.app_id.clone(), game_state);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to glob ACF files: {}", e);
                    }
                }
            }
        }

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
            if let Some(new_state) = self.parse_acf_file(&game_state.manifest_path) {
                if self.is_updating(&new_state) {
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

    fn parse_acf_file(&self, path: &PathBuf) -> Option<GameUpdateState> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return None,
        };

        let mut app_id = String::new();
        let mut name = String::new();
        let mut state_flags: u32 = 0;

        for line in content.lines() {
            let trimmed = line.trim();
            
            if trimmed.starts_with("\"appid\"") || trimmed.starts_with("\"AppID\"") {
                if let Some(val) = self.extract_vdf_value(trimmed) {
                    app_id = val;
                }
            } else if trimmed.starts_with("\"name\"") {
                if let Some(val) = self.extract_vdf_value(trimmed) {
                    name = val;
                }
            } else if trimmed.starts_with("\"StateFlags\"") || trimmed.starts_with("\"stateflags\"") {
                if let Some(val) = self.extract_vdf_value(trimmed) {
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
            manifest_path: path.clone(),
        })
    }

    fn extract_vdf_value(&self, line: &str) -> Option<String> {
        // Format: "key"		"value"
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() >= 4 {
            Some(parts[3].to_string())
        } else {
            None
        }
    }

    fn is_updating(&self, game: &GameUpdateState) -> bool {
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

    async fn publish_state(&self) {
        let is_updating = !self.updating_games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        
        self.state.mqtt.publish_sensor_retained("steam_updating", state_str).await;

        // Publish attributes with game names
        if is_updating {
            let names: Vec<&str> = self.updating_games.values().map(|g| g.name.as_str()).collect();
            let attrs = serde_json::json!({
                "updating_games": names,
                "count": self.updating_games.len()
            });
            self.state.mqtt.publish_sensor_attributes("steam_updating", &attrs).await;
        } else {
            let attrs = serde_json::json!({
                "updating_games": [],
                "count": 0
            });
            self.state.mqtt.publish_sensor_attributes("steam_updating", &attrs).await;
        }
    }
}
