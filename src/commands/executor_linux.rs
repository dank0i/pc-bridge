//! Command executor for Linux — feature-parity with Windows executor

use log::{debug, error, info, warn};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::custom::execute_custom_command;
use super::launcher_linux::expand_launcher_shortcut;
use crate::AppState;
use crate::audio::{self, MediaKey};
use crate::mqtt::CommandReceiver;
use crate::notification;
use crate::power::sync_mqtt::{SyncMqttConfig, parse_broker_url, sync_mqtt_publish_sleep};
use crate::power::wake_display;
use crate::steam::SteamGameDiscovery;

const MAX_CONCURRENT_COMMANDS: usize = 5;

/// Predefined shell commands for Linux
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some("xdg-screensaver activate"),
        "Wake" | "Sleep" | "Hibernate" => None, // Handled natively
        "Shutdown" => Some("systemctl poweroff"),
        "Lock" => Some("loginctl lock-session"),
        "Restart" => Some("systemctl reboot"),
        _ => None,
    }
}

pub struct CommandExecutor {
    state: Arc<AppState>,
    command_rx: CommandReceiver,
    semaphore: Arc<Semaphore>,
}

impl CommandExecutor {
    pub fn new(state: Arc<AppState>, command_rx: CommandReceiver) -> Self {
        Self {
            state,
            command_rx,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS)),
        }
    }

    pub async fn run(mut self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Command executor shutting down");
                    break;
                }
                Some(cmd) = self.command_rx.recv() => {
                    // Rate limit with semaphore
                    let permit = match self.semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            warn!("Command rate limited, dropping: {}", cmd.name);
                            continue;
                        }
                    };

                    let state_clone = self.state.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = Self::execute_command(&cmd.name, &cmd.payload, &state_clone).await {
                            error!("Command error: {}", e);
                        }
                    });
                }
            }
        }
    }

    async fn execute_command(
        name: &str,
        payload: &str,
        state: &Arc<AppState>,
    ) -> anyhow::Result<()> {
        let payload = payload.trim();
        let payload = if payload.eq_ignore_ascii_case("PRESS") {
            ""
        } else {
            payload
        };

        info!("Executing command: {} (payload: {:?})", name, payload);

        // ── Native commands (no shell needed) ──────────────────────────
        match name {
            "DiscordLeaveChannel" => {
                let keybind = state
                    .config
                    .read()
                    .await
                    .discord_keybind
                    .clone()
                    .unwrap_or_else(|| "ctrl+f6".to_string());
                send_keybind_linux(&keybind);
                return Ok(());
            }
            "Wake" => {
                wake_display();
                return Ok(());
            }
            "Sleep" | "Hibernate" => {
                // Pre-publish sleep state via sync TCP before the NIC goes down,
                // matching the Windows behavior in power/events.rs.
                let cfg = {
                    let config = state.config.read().await;
                    let (host, port, use_tls) = parse_broker_url(&config.mqtt.broker);
                    SyncMqttConfig {
                        host,
                        port,
                        use_tls,
                        user: config.mqtt.user.clone(),
                        pass: config.mqtt.pass.clone(),
                        client_id: format!("{}-sleep", config.client_id()),
                        sleep_topic: format!(
                            "homeassistant/sensor/{}/sleep_state/state",
                            config.device_name
                        ),
                    }
                };
                match sync_mqtt_publish_sleep(&cfg) {
                    Ok(()) => info!("Sleep state pre-published via sync TCP"),
                    Err(e) => warn!("Sync MQTT sleep pre-publish failed: {}", e),
                }
                // Also publish via async client as fallback
                state
                    .mqtt
                    .publish_sensor_retained("sleep_state", "sleeping")
                    .await;
                let cmd = if name == "Sleep" {
                    "systemctl suspend"
                } else {
                    "systemctl hibernate"
                };
                let _ = Command::new("bash").args(["-c", cmd]).spawn();
                return Ok(());
            }
            "notification" => {
                if !payload.is_empty() {
                    notification::show_toast(payload)?;
                }
                return Ok(());
            }
            "VolumeSet" => {
                if let Ok(level) = payload.parse::<f32>() {
                    tokio::task::spawn_blocking(move || audio::set_volume(level));
                }
                return Ok(());
            }
            "VolumeMute" => {
                if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                    tokio::task::spawn_blocking(audio::toggle_mute);
                } else {
                    let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
                    tokio::task::spawn_blocking(move || audio::set_mute(mute));
                }
                return Ok(());
            }
            "MediaPlayPause" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::PlayPause));
                return Ok(());
            }
            "MediaNext" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Next));
                return Ok(());
            }
            "MediaPrevious" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Previous));
                return Ok(());
            }
            "MediaStop" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Stop));
                return Ok(());
            }
            "RefreshSteamGames" => {
                info!("Refreshing Steam game library...");
                match SteamGameDiscovery::discover_async().await {
                    Some(discovery) => {
                        let mut config = state.config.write().await;
                        match config.merge_steam_games(&discovery) {
                            Ok((added, removed)) if added > 0 || removed > 0 => {
                                info!(
                                    "Steam refresh: +{} added, -{} removed ({}ms{})",
                                    added,
                                    removed,
                                    discovery.build_time_ms,
                                    if discovery.from_cache { ", cached" } else { "" }
                                );
                                drop(config);
                                let _ = state.config_generation.send(());
                            }
                            Ok(_) => {
                                info!(
                                    "Steam refresh: no changes ({}ms{})",
                                    discovery.build_time_ms,
                                    if discovery.from_cache { ", cached" } else { "" }
                                );
                            }
                            Err(e) => {
                                warn!("Steam refresh: failed to save games: {}", e);
                            }
                        }
                    }
                    None => {
                        info!("Steam refresh: Steam not found or no games installed");
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        // ── Custom commands ────────────────────────────────────────────
        if execute_custom_command(state, name).await? {
            return Ok(());
        }

        // ── Shell commands (predefined → launcher → raw → not found) ─────────────
        let cmd_str = match get_predefined_command(name) {
            Some(cmd) => cmd.to_string(),
            None => {
                // Try launcher shortcuts (always allowed — validated and safe)
                if let Some(expanded) = expand_launcher_shortcut(payload) {
                    expanded
                } else if !payload.is_empty() {
                    let config = state.config.read().await;
                    if !config.allow_raw_commands {
                        warn!("Raw command blocked (allow_raw_commands=false): {}", name);
                        return Ok(());
                    }
                    payload.to_string()
                } else {
                    warn!("No command configured for: {}", name);
                    return Ok(());
                }
            }
        };

        info!("Running: {}", cmd_str);

        // Execute via bash
        let mut child = Command::new("bash").args(["-c", &cmd_str]).spawn()?;

        // Wait with timeout in background
        tokio::spawn(async move {
            match tokio::time::timeout(
                std::time::Duration::from_mins(5),
                tokio::task::spawn_blocking(move || child.wait()),
            )
            .await
            {
                Ok(Ok(Ok(status))) => {
                    if !status.success() {
                        warn!("Command exited with: {}", status);
                    }
                }
                Ok(Ok(Err(e))) => error!("Command wait error: {}", e),
                Ok(Err(e)) => error!("Task join error: {}", e),
                Err(_) => warn!("Command timed out after 5 minutes"),
            }
        });

        Ok(())
    }
}

/// Send a keybind via xdotool (Linux equivalent of Windows keybd_event).
///
/// Converts our format ("ctrl+f6") to xdotool format ("ctrl+F6").
fn send_keybind_linux(keybind: &str) {
    let xdotool_keybind: String = keybind
        .split('+')
        .map(|part| {
            let lower = part.trim().to_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => "ctrl".to_string(),
                "shift" => "shift".to_string(),
                "alt" => "alt".to_string(),
                "win" | "super" => "super".to_string(),
                // Function keys: xdotool expects uppercase F
                k if k.starts_with('f') && k[1..].parse::<u32>().is_ok() => k.to_uppercase(),
                k => k.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("+");

    info!("Sending keybind via xdotool: {}", xdotool_keybind);
    match Command::new("xdotool")
        .args(["key", &xdotool_keybind])
        .spawn()
    {
        Ok(_) => {}
        Err(e) => warn!("Failed to send keybind via xdotool: {}", e),
    }
}
