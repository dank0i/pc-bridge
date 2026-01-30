//! Command executor - handles commands from Home Assistant

use std::sync::Arc;
use std::process::Command;
use std::os::windows::process::CommandExt;
use tokio::sync::Semaphore;
use tracing::{info, warn, error, debug};

use crate::AppState;
use crate::mqtt::CommandReceiver;
use crate::power::wake_display;
use crate::notification;
use super::launcher::expand_launcher_shortcut;
use super::custom::execute_custom_command;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const MAX_CONCURRENT_COMMANDS: usize = 5;

/// Predefined commands
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some(r#"%windir%\System32\scrnsave.scr /s"#),
        "Wake" => None, // Handled specially
        "Shutdown" => Some("shutdown -s -t 0"),
        "sleep" => Some("Rundll32.exe powrprof.dll,SetSuspendState 0,1,0"),
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

                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        let _permit = permit; // Keep permit alive until done
                        if let Err(e) = Self::execute_command(&cmd.name, &cmd.payload, &state).await {
                            error!("Command error: {}", e);
                        }
                    });
                }
            }
        }
    }

    async fn execute_command(name: &str, payload: &str, state: &Arc<AppState>) -> anyhow::Result<()> {
        // Normalize payload
        let payload = payload.trim();
        let payload = if payload.eq_ignore_ascii_case("PRESS") { "" } else { payload };

        info!("Executing command: {} (payload: {:?})", name, payload);

        match name {
            "discord_leave_channel" => {
                send_ctrl_f6();
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
            _ => {}
        }

        // Check for custom command first
        if execute_custom_command(state, name).await? {
            return Ok(());
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

        // Expand launcher shortcuts (steam:, epic:, exe:, etc.)
        let cmd_str = expand_launcher_shortcut(&cmd_str).unwrap_or(cmd_str);

        // Expand environment variables
        let cmd_str = expand_env_vars(&cmd_str);

        info!("Running: {}", cmd_str);

        // Build PowerShell command
        let ps_cmd = if needs_ampersand(&cmd_str) {
            format!("& {}", cmd_str)
        } else {
            cmd_str
        };

        // Execute via PowerShell
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .creation_flags(CREATE_NO_WINDOW)
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
                Err(_) => {
                    warn!("Command timed out after 5 minutes");
                    // Process already dropped, will be cleaned up
                }
            }
        });

        Ok(())
    }
}

/// Check if command needs "& " prefix for PowerShell
fn needs_ampersand(cmd: &str) -> bool {
    let ps_cmdlets = ["Start-Process", "Add-Type", "Get-Process", "Stop-Process", 
                       "Set-", "Get-", "New-", "Remove-", "Invoke-"];
    !ps_cmdlets.iter().any(|prefix| cmd.starts_with(prefix))
}

/// Expand Windows-style %VAR% environment variables
fn expand_env_vars(s: &str) -> String {
    let mut result = s.to_string();
    
    while let Some(start) = result.find('%') {
        if let Some(end) = result[start + 1..].find('%') {
            let end = start + 1 + end;
            let var_name = &result[start + 1..end];
            let value = std::env::var(var_name).unwrap_or_default();
            result = format!("{}{}{}", &result[..start], value, &result[end + 1..]);
        } else {
            break;
        }
    }
    
    result
}

/// Send Ctrl+F6 keypress (Discord leave channel hotkey)
fn send_ctrl_f6() {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    const VK_CONTROL: u8 = 0x11;
    const VK_F6: u8 = 0x75;

    unsafe {
        // Key down Ctrl
        keybd_event(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(std::time::Duration::from_millis(10));
        
        // Key down F6
        keybd_event(VK_F6, 0, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(std::time::Duration::from_millis(10));
        
        // Key up F6
        keybd_event(VK_F6, 0, KEYEVENTF_KEYUP, 0);
        std::thread::sleep(std::time::Duration::from_millis(10));
        
        // Key up Ctrl
        keybd_event(VK_CONTROL, 0, KEYEVENTF_KEYUP, 0);
    }
}
