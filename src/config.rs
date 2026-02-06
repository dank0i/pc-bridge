//! Configuration loading and hot-reload support

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::{Context, Result, bail};
use notify::{Watcher, RecursiveMode, Event, EventKind};
use serde::{Deserialize, Serialize};
use tracing::{info, warn, error};

use crate::AppState;

/// Default config template (embedded in binary)
const DEFAULT_CONFIG: &str = r#"{
    "device_name": "my-pc",
    "mqtt": {
        "broker": "tcp://homeassistant.local:1883",
        "user": "",
        "pass": ""
    },
    "intervals": {
        "game_sensor": 5,
        "last_active": 10,
        "availability": 30
    },
    "features": {
        "game_detection": false,
        "idle_tracking": false,
        "power_events": false,
        "notifications": false
    },
    "games": {},
    "show_tray_icon": true,
    "custom_sensors_enabled": false,
    "custom_commands_enabled": false,
    "custom_command_privileges_allowed": false,
    "custom_sensors": [],
    "custom_commands": []
}"#;

/// User configuration structure (matches userConfig.json)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub device_name: String,
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub intervals: IntervalConfig,
    #[serde(default)]
    pub features: FeatureConfig,
    /// Games map: process_pattern → GameConfig
    /// Can be simple string (game_id) or object with app_id
    #[serde(default)]
    pub games: HashMap<String, GameConfig>,
    
    // Tray icon - enabled by default
    #[serde(default = "default_true")]
    pub show_tray_icon: bool,
    
    // Custom sensors/commands - disabled by default for security
    #[serde(default)]
    pub custom_sensors_enabled: bool,
    #[serde(default)]
    pub custom_commands_enabled: bool,
    #[serde(default)]
    pub custom_command_privileges_allowed: bool,
    #[serde(default)]
    pub custom_sensors: Vec<CustomSensor>,
    #[serde(default)]
    pub custom_commands: Vec<CustomCommand>,
}

fn default_true() -> bool { true }

/// Game configuration - supports both simple string and object with app_id
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GameConfig {
    /// Simple: just the game ID string
    Simple(String),
    /// Full: game ID with optional Steam app_id
    Full {
        game_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        app_id: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Whether this was auto-discovered from Steam
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auto_discovered: bool,
    },
}

impl GameConfig {
    /// Get the game_id regardless of variant
    pub fn game_id(&self) -> &str {
        match self {
            GameConfig::Simple(id) => id,
            GameConfig::Full { game_id, .. } => game_id,
        }
    }
    
    /// Get display name (falls back to smart title-cased game_id if no name set)
    pub fn display_name(&self) -> String {
        match self {
            GameConfig::Simple(id) => Self::smart_title(id),
            GameConfig::Full { name, game_id, .. } => {
                name.clone().unwrap_or_else(|| Self::smart_title(game_id))
            }
        }
    }
    
    /// Convert game_id to display name with smart casing
    fn smart_title(game_id: &str) -> String {
        game_id
            .replace('_', " ")
            .split_whitespace()
            .map(|word| {
                // Keep numbers as-is, capitalize first letter of words
                if word.chars().next().map(|c| c.is_numeric()).unwrap_or(false) {
                    word.to_string()
                } else {
                    let mut chars = word.chars();
                    match chars.next() {
                        None => String::new(),
                        Some(first) => first.to_uppercase().chain(chars).collect(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    
    /// Get app_id if available
    pub fn app_id(&self) -> Option<u32> {
        match self {
            GameConfig::Simple(_) => None,
            GameConfig::Full { app_id, .. } => *app_id,
        }
    }
    
    /// Create from Steam discovery
    pub fn from_steam(game_id: String, app_id: u32, name: String) -> Self {
        GameConfig::Full {
            game_id,
            app_id: Some(app_id),
            name: Some(name),
            auto_discovered: true,
        }
    }
}

/// Feature toggles - all disabled by default (opt-in philosophy)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeatureConfig {
    #[serde(default)]
    pub game_detection: bool,
    #[serde(default)]
    pub idle_tracking: bool,
    #[serde(default)]
    pub power_events: bool,
    #[serde(default)]
    pub notifications: bool,
    #[serde(default)]
    pub system_sensors: bool,
    #[serde(default)]
    pub audio_control: bool,
}

/// Custom sensor definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSensor {
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: CustomSensorType,
    #[serde(default = "default_sensor_interval")]
    pub interval_seconds: u64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    // Type-specific fields
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub registry_key: Option<String>,
    #[serde(default)]
    pub registry_value: Option<String>,
}

/// Custom sensor types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CustomSensorType {
    Powershell,
    ProcessExists,
    FileContents,
    Registry,
}

fn default_sensor_interval() -> u64 { 30 }

/// Custom command definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomCommand {
    pub name: String,
    #[serde(rename = "type")]
    pub command_type: CustomCommandType,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub admin: bool,
    // Type-specific fields
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub command: Option<String>,
}

/// Custom command types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CustomCommandType {
    Powershell,
    Executable,
    Shell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttConfig {
    pub broker: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    #[serde(default)]
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalConfig {
    #[serde(default = "default_game_sensor")]
    pub game_sensor: u64,
    #[serde(default = "default_last_active")]
    pub last_active: u64,
    #[serde(default = "default_availability")]
    pub availability: u64,
}

impl Default for IntervalConfig {
    fn default() -> Self {
        Self {
            game_sensor: default_game_sensor(),
            last_active: default_last_active(),
            availability: default_availability(),
        }
    }
}

fn default_game_sensor() -> u64 { 5 }
fn default_last_active() -> u64 { 10 }
fn default_availability() -> u64 { 30 }

impl Config {
    /// Check if this is a first run (no config file exists)
    pub fn is_first_run() -> Result<bool> {
        let config_path = Self::config_path()?;
        Ok(!config_path.exists())
    }
    
    /// Load configuration from userConfig.json next to the executable
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            bail!(
                "Configuration file not found at {:?}\n\
                 Run the setup wizard first.",
                config_path
            );
        }

        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {:?}", config_path))?;

        // Migrate config if needed (adds missing fields)
        let content = Self::migrate_config(&config_path, &content)?;

        let config: Config = serde_json::from_str(&content)
            .with_context(|| "Failed to parse userConfig.json")?;

        config.validate()?;

        Ok(config)
    }

    /// Migrate config by adding missing fields with defaults
    fn migrate_config(config_path: &PathBuf, content: &str) -> Result<String> {
        let mut json: serde_json::Value = serde_json::from_str(content)
            .with_context(|| "Failed to parse config as JSON")?;
        
        let obj = json.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Config must be a JSON object"))?;
        
        let mut migrated = false;
        
        // Add show_tray_icon if missing (default true)
        if !obj.contains_key("show_tray_icon") {
            obj.insert("show_tray_icon".to_string(), serde_json::Value::Bool(true));
            migrated = true;
        }
        
        // Fix zero intervals (bug from v1.9.0-beta.1/2)
        if let Some(intervals) = obj.get_mut("intervals").and_then(|v| v.as_object_mut()) {
            if intervals.get("game_sensor").and_then(|v| v.as_u64()) == Some(0) {
                intervals.insert("game_sensor".to_string(), serde_json::json!(5));
                migrated = true;
            }
            if intervals.get("last_active").and_then(|v| v.as_u64()) == Some(0) {
                intervals.insert("last_active".to_string(), serde_json::json!(10));
                migrated = true;
            }
            if intervals.get("availability").and_then(|v| v.as_u64()) == Some(0) {
                intervals.insert("availability".to_string(), serde_json::json!(30));
                migrated = true;
            }
        }
        
        // Add features section if missing (all disabled by default)
        if !obj.contains_key("features") {
            obj.insert("features".to_string(), serde_json::json!({
                "game_detection": false,
                "idle_tracking": false,
                "power_events": false,
                "notifications": false
            }));
            migrated = true;
        }
        
        // Add custom sensor/command fields if missing
        if !obj.contains_key("custom_sensors_enabled") {
            obj.insert("custom_sensors_enabled".to_string(), serde_json::Value::Bool(false));
            migrated = true;
        }
        if !obj.contains_key("custom_commands_enabled") {
            obj.insert("custom_commands_enabled".to_string(), serde_json::Value::Bool(false));
            migrated = true;
        }
        if !obj.contains_key("custom_command_privileges_allowed") {
            obj.insert("custom_command_privileges_allowed".to_string(), serde_json::Value::Bool(false));
            migrated = true;
        }
        if !obj.contains_key("custom_sensors") {
            obj.insert("custom_sensors".to_string(), serde_json::Value::Array(vec![]));
            migrated = true;
        }
        if !obj.contains_key("custom_commands") {
            obj.insert("custom_commands".to_string(), serde_json::Value::Array(vec![]));
            migrated = true;
        }
        
        if migrated {
            // Write back the migrated config
            let new_content = serde_json::to_string_pretty(&json)?;
            std::fs::write(config_path, &new_content)
                .with_context(|| format!("Failed to write migrated config to {:?}", config_path))?;
            info!("Migrated userConfig.json - added new fields");
            Ok(new_content)
        } else {
            Ok(content.to_string())
        }
    }

    /// Get the path to userConfig.json
    pub fn config_path() -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().context("No parent directory")?;
        Ok(dir.join("userConfig.json"))
    }
    
    /// Merge Steam-discovered games into the config and save
    /// 
    /// Only adds games that don't already exist (by process pattern).
    /// Returns the number of new games added.
    pub fn merge_steam_games(&mut self, steam_games: &crate::steam::SteamGameDiscovery) -> Result<usize> {
        let mut added = 0;
        
        for (exe_key, game) in &steam_games.games {
            // exe_key is already lowercase, no extension (e.g., "cs2")
            // Check if this pattern already exists
            if self.games.contains_key(exe_key) {
                continue;
            }
            
            // Generate game_id from name - only allow ASCII alphanumeric and underscore
            let game_id: String = game.name
                .to_lowercase()
                .chars()
                .filter_map(|c| {
                    if c.is_ascii_alphanumeric() {
                        Some(c)
                    } else if c == ' ' || c == '-' {
                        Some('_')
                    } else {
                        None // Strip ™, ®, :, ', etc.
                    }
                })
                .collect();
            
            // Add to games map
            self.games.insert(
                exe_key.clone(),
                GameConfig::from_steam(game_id, game.app_id, game.name.clone()),
            );
            added += 1;
        }
        
        if added > 0 {
            // Save updated config
            self.save()?;
        }
        
        Ok(added)
    }
    
    /// Save current config to userConfig.json
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&config_path, content)
            .with_context(|| format!("Failed to write config to {:?}", config_path))?;
        Ok(())
    }

    /// Validate configuration values
    fn validate(&self) -> Result<()> {
        if self.device_name.is_empty() {
            bail!("device_name is required");
        }
        if self.device_name == "my-pc" {
            bail!("device_name is still the default 'my-pc' - please change it");
        }
        if self.device_name.contains(char::is_whitespace) {
            bail!("device_name cannot contain whitespace");
        }
        if self.mqtt.broker.is_empty() {
            bail!("mqtt.broker is required");
        }
        if !self.mqtt.broker.starts_with("tcp://") 
            && !self.mqtt.broker.starts_with("ssl://")
            && !self.mqtt.broker.starts_with("ws://")
            && !self.mqtt.broker.starts_with("wss://") 
        {
            bail!("mqtt.broker must start with tcp://, ssl://, ws://, or wss://");
        }

        // Validate custom sensors
        for sensor in &self.custom_sensors {
            Self::validate_custom_sensor(sensor)?;
        }

        // Validate custom commands
        for cmd in &self.custom_commands {
            Self::validate_custom_command(cmd, self.custom_command_privileges_allowed)?;
        }

        Ok(())
    }

    /// Validate a custom sensor definition
    fn validate_custom_sensor(sensor: &CustomSensor) -> Result<()> {
        if sensor.name.is_empty() {
            bail!("Custom sensor name cannot be empty");
        }
        if sensor.name.contains(char::is_whitespace) {
            bail!("Custom sensor name '{}' cannot contain whitespace", sensor.name);
        }
        
        match sensor.sensor_type {
            CustomSensorType::Powershell => {
                if sensor.script.is_none() || sensor.script.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom sensor '{}' (powershell) requires 'script' field", sensor.name);
                }
            }
            CustomSensorType::ProcessExists => {
                if sensor.process.is_none() || sensor.process.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom sensor '{}' (process_exists) requires 'process' field", sensor.name);
                }
            }
            CustomSensorType::FileContents => {
                if sensor.file_path.is_none() || sensor.file_path.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom sensor '{}' (file_contents) requires 'file_path' field", sensor.name);
                }
            }
            CustomSensorType::Registry => {
                if sensor.registry_key.is_none() || sensor.registry_key.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom sensor '{}' (registry) requires 'registry_key' field", sensor.name);
                }
                if sensor.registry_value.is_none() || sensor.registry_value.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom sensor '{}' (registry) requires 'registry_value' field", sensor.name);
                }
            }
        }
        
        Ok(())
    }

    /// Validate a custom command definition
    fn validate_custom_command(cmd: &CustomCommand, privileges_allowed: bool) -> Result<()> {
        if cmd.name.is_empty() {
            bail!("Custom command name cannot be empty");
        }
        if cmd.name.contains(char::is_whitespace) {
            bail!("Custom command name '{}' cannot contain whitespace", cmd.name);
        }
        
        if cmd.admin && !privileges_allowed {
            bail!(
                "Custom command '{}' has admin=true but custom_command_privileges_allowed=false. \
                 Set custom_command_privileges_allowed=true if you understand the security implications.",
                cmd.name
            );
        }
        
        match cmd.command_type {
            CustomCommandType::Powershell => {
                if cmd.script.is_none() || cmd.script.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom command '{}' (powershell) requires 'script' field", cmd.name);
                }
            }
            CustomCommandType::Executable => {
                if cmd.path.is_none() || cmd.path.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom command '{}' (executable) requires 'path' field", cmd.name);
                }
            }
            CustomCommandType::Shell => {
                if cmd.command.is_none() || cmd.command.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                    bail!("Custom command '{}' (shell) requires 'command' field", cmd.name);
                }
            }
        }
        
        Ok(())
    }

    /// Get device ID (device_name with dashes replaced by underscores)
    pub fn device_id(&self) -> String {
        self.device_name.replace('-', "_")
    }

    /// Show alert to user about config (platform-specific)
    #[cfg(windows)]
    fn show_config_alert(path: &PathBuf) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::w;
        
        let message = format!(
            "Welcome to PC Bridge!\n\n\
             A default configuration file has been created at:\n\
             {}\n\n\
             Please edit this file with your MQTT broker settings \
             and device name, then restart the application.",
            path.display()
        );
        
        let wide_message: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        
        unsafe {
            MessageBoxW(
                None,
                windows::core::PCWSTR::from_raw(wide_message.as_ptr()),
                w!("PC Bridge - Configuration Required"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    #[cfg(unix)]
    fn show_config_alert(path: &PathBuf) {
        // On Unix, just print to stderr (no GUI assumed)
        eprintln!(
            "Welcome to PC Bridge!\n\n\
             A default configuration file has been created at:\n\
             {}\n\n\
             Please edit this file with your MQTT broker settings \
             and device name, then restart the application.",
            path.display()
        );
    }

    /// Get MQTT client ID
    pub fn client_id(&self) -> String {
        self.mqtt.client_id
            .clone()
            .unwrap_or_else(|| format!("pc-agent-{}", self.device_name))
    }
}

/// Watch userConfig.json for changes and reload games on modification
pub async fn watch_config(state: Arc<AppState>) {
    let config_path = match Config::config_path() {
        Ok(p) => p,
        Err(e) => {
            error!("Cannot watch config: {}", e);
            return;
        }
    };

    let dir = match config_path.parent() {
        Some(d) => d.to_path_buf(),
        None => {
            error!("Cannot get config directory");
            return;
        }
    };

    let filename = config_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let (tx, mut rx) = tokio::sync::mpsc::channel(10);

    // Create watcher in a blocking task since notify isn't async
    let filename_clone = filename.clone();
    let _watch_handle = tokio::task::spawn_blocking(move || {
        let tx = tx;
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            if let Ok(event) = res {
                let _ = tx.blocking_send(event);
            }
        }).expect("Failed to create file watcher");

        watcher.watch(&dir, RecursiveMode::NonRecursive)
            .expect("Failed to watch directory");

        info!("Watching for changes to {:?}", dir.join(&filename_clone));

        // Keep watcher alive
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });

    let mut shutdown_rx = state.shutdown_tx.subscribe();

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("Config watcher shutting down");
                break;
            }
            Some(event) = rx.recv() => {
                // Check if it's our file
                let is_our_file = event.paths.iter().any(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy() == filename)
                        .unwrap_or(false)
                });

                if is_our_file {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            info!("Config file changed, reloading...");
                            reload_games(&state).await;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Reload just the games section from config
async fn reload_games(state: &AppState) {
    match Config::load() {
        Ok(new_config) => {
            let mut config = state.config.write().await;
            let old_count = config.games.len();
            config.games = new_config.games;
            
            // Also reload custom sensors/commands config
            let old_sensors_enabled = config.custom_sensors_enabled;
            let old_commands_enabled = config.custom_commands_enabled;
            
            config.custom_sensors_enabled = new_config.custom_sensors_enabled;
            config.custom_commands_enabled = new_config.custom_commands_enabled;
            config.custom_command_privileges_allowed = new_config.custom_command_privileges_allowed;
            config.custom_sensors = new_config.custom_sensors;
            config.custom_commands = new_config.custom_commands;
            
            info!("Reloaded games: {} (was {})", config.games.len(), old_count);
            
            // Log security-relevant changes
            if config.custom_sensors_enabled != old_sensors_enabled {
                if config.custom_sensors_enabled {
                    warn!("⚠️  custom_sensors_enabled is now TRUE");
                } else {
                    info!("custom_sensors_enabled is now false");
                }
            }
            if config.custom_commands_enabled != old_commands_enabled {
                if config.custom_commands_enabled {
                    warn!("⚠️  custom_commands_enabled is now TRUE - arbitrary code execution possible via MQTT");
                } else {
                    info!("custom_commands_enabled is now false");
                }
            }
            if config.custom_command_privileges_allowed {
                warn!("⚠️  custom_command_privileges_allowed is TRUE - commands can run with ADMIN privileges");
            }
        }
        Err(e) => {
            warn!("Failed to reload config: {}", e);
        }
    }
}
