//! Command executor for Linux

use std::sync::Arc;
use std::process::Command;
use tokio::sync::Semaphore;
use tracing::{info, warn, error, debug};

use crate::AppState;
use crate::mqtt::CommandReceiver;
use crate::power::wake_display;

const MAX_CONCURRENT_COMMANDS: usize = 5;

/// Predefined commands for Linux
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some("xdg-screensaver activate"),
        "Wake" => None, // Handled specially
        "Shutdown" => Some("systemctl poweroff"),
        "sleep" => Some("systemctl suspend"),
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

                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = Self::execute_command(&cmd.name, &cmd.payload).await {
                            error!("Command error: {}", e);
                        }
                    });
                }
            }
        }
    }

    async fn execute_command(name: &str, payload: &str) -> anyhow::Result<()> {
        let payload = payload.trim();
        let payload = if payload.eq_ignore_ascii_case("PRESS") { "" } else { payload };

        info!("Executing command: {} (payload: {:?})", name, payload);

        match name {
            "Wake" => {
                wake_display();
                return Ok(());
            }
            "notification" => {
                if !payload.is_empty() {
                    // Use notify-send for desktop notifications
                    let _ = Command::new("notify-send")
                        .args(["PC Bridge", payload])
                        .spawn();
                }
                return Ok(());
            }
            _ => {}
        }

        // Get command string
        let cmd_str = match get_predefined_command(name) {
            Some(cmd) => cmd.to_string(),
            None if !payload.is_empty() => payload.to_string(),
            None => {
                warn!("No command configured for: {}", name);
                return Ok(());
            }
        };

        info!("Running: {}", cmd_str);

        // Execute via bash
        let mut child = Command::new("bash")
            .args(["-c", &cmd_str])
            .spawn()?;

        // Wait with timeout in background
        tokio::spawn(async move {
            match tokio::time::timeout(
                std::time::Duration::from_secs(300),
                tokio::task::spawn_blocking(move || child.wait())
            ).await {
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
