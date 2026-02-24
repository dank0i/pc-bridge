//! Command executor for Linux — feature-parity with Windows executor

use log::{debug, error, info, warn};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::custom::execute_custom_command;
use crate::AppState;
use crate::audio::{self, MediaKey};
use crate::mqtt::CommandReceiver;
use crate::notification;
use crate::power::wake_display;

const MAX_CONCURRENT_COMMANDS: usize = 5;

/// Predefined shell commands for Linux
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some("xdg-screensaver activate"),
        "Wake" => None, // Handled natively
        "Shutdown" => Some("systemctl poweroff"),
        "sleep" => Some("systemctl suspend"),
        "Lock" => Some("loginctl lock-session"),
        "Hibernate" => Some("systemctl hibernate"),
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
            "discord_leave_channel" => {
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
            "notification" => {
                if !payload.is_empty() {
                    notification::show_toast(payload)?;
                }
                return Ok(());
            }
            "volume_set" => {
                if let Ok(level) = payload.parse::<f32>() {
                    tokio::task::spawn_blocking(move || audio::set_volume(level));
                }
                return Ok(());
            }
            "volume_mute" => {
                if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                    tokio::task::spawn_blocking(audio::toggle_mute);
                } else {
                    let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
                    tokio::task::spawn_blocking(move || audio::set_mute(mute));
                }
                return Ok(());
            }
            "volume_toggle_mute" => {
                tokio::task::spawn_blocking(audio::toggle_mute);
                return Ok(());
            }
            "media_play_pause" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::PlayPause));
                return Ok(());
            }
            "media_next" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Next));
                return Ok(());
            }
            "media_previous" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Previous));
                return Ok(());
            }
            "media_stop" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Stop));
                return Ok(());
            }
            _ => {}
        }

        // ── Custom commands ────────────────────────────────────────────
        if execute_custom_command(state, name).await? {
            return Ok(());
        }

        // ── Shell commands (predefined → raw → not found) ─────────────
        let cmd_str = match get_predefined_command(name) {
            Some(cmd) => cmd.to_string(),
            None if !payload.is_empty() => {
                let config = state.config.read().await;
                if !config.allow_raw_commands {
                    warn!("Raw command blocked (allow_raw_commands=false): {}", name);
                    return Ok(());
                }
                payload.to_string()
            }
            None => {
                warn!("No command configured for: {}", name);
                return Ok(());
            }
        };

        info!("Running: {}", cmd_str);

        // Execute via bash
        let mut child = Command::new("bash").args(["-c", &cmd_str]).spawn()?;

        // Wait with timeout in background
        tokio::spawn(async move {
            match tokio::time::timeout(
                std::time::Duration::from_secs(300),
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
