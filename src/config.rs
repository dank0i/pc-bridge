//! Configuration loading and hot-reload support

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::{Context, Result, bail};
use notify::{Watcher, RecursiveMode, Event, EventKind};
use serde::Deserialize;
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
    "games": {
        "example_game": "ExampleGame.exe"
    }
}"#;

/// User configuration structure (matches userConfig.json)
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub device_name: String,
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub intervals: IntervalConfig,
    #[serde(default)]
    pub games: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    pub broker: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    #[serde(default)]
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct IntervalConfig {
    #[serde(default = "default_game_sensor")]
    pub game_sensor: u64,
    #[serde(default = "default_last_active")]
    pub last_active: u64,
    #[serde(default = "default_availability")]
    pub availability: u64,
}

fn default_game_sensor() -> u64 { 5 }
fn default_last_active() -> u64 { 10 }
fn default_availability() -> u64 { 30 }

impl Config {
    /// Load configuration from userConfig.json next to the executable
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            // Create default config file
            std::fs::write(&config_path, DEFAULT_CONFIG)
                .with_context(|| format!("Failed to create {:?}", config_path))?;
            
            info!("Created default userConfig.json at {:?}", config_path);
            
            // Show alert to user
            Self::show_config_alert(&config_path);
            
            bail!(
                "userConfig.json created at {:?}\n\
                 Please edit it with your settings and restart.",
                config_path
            );
        }

        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {:?}", config_path))?;

        let config: Config = serde_json::from_str(&content)
            .with_context(|| "Failed to parse userConfig.json")?;

        config.validate()?;

        Ok(config)
    }

    /// Get the path to userConfig.json
    pub fn config_path() -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().context("No parent directory")?;
        Ok(dir.join("userConfig.json"))
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

        Ok(())
    }

    /// Get device ID (device_name with dashes replaced by underscores)
    pub fn device_id(&self) -> String {
        self.device_name.replace('-', "_")
    }

    /// Show Windows message box alerting user to configure
    fn show_config_alert(path: &PathBuf) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::w;
        
        let message = format!(
            "Welcome to PC Agent!\n\n\
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
                w!("PC Agent - Configuration Required"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
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
            info!("Reloaded games: {} (was {})", config.games.len(), old_count);
        }
        Err(e) => {
            warn!("Failed to reload config: {}", e);
        }
    }
}
