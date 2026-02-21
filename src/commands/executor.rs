//! Command executor - handles commands from Home Assistant

use log::{debug, error, info, warn};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::custom::execute_custom_command;
use super::launcher::expand_launcher_shortcut;
use crate::AppState;
use crate::audio::{self, MediaKey};
use crate::mqtt::CommandReceiver;
use crate::notification;
use crate::power::wake_display;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const MAX_CONCURRENT_COMMANDS: usize = 5;

/// Predefined commands
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some(r#"%windir%\System32\scrnsave.scr /s"#),
        // These are handled natively in execute_command
        "Wake" | "Lock" | "Hibernate" | "Restart" | "volume_set" | "volume_mute"
        | "media_play_pause" | "media_next" | "media_previous" | "media_stop" => None,
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

    async fn execute_command(
        name: &str,
        payload: &str,
        state: &Arc<AppState>,
    ) -> anyhow::Result<()> {
        // Normalize payload
        let payload = payload.trim();
        let payload = if payload.eq_ignore_ascii_case("PRESS") {
            ""
        } else {
            payload
        };

        info!("Executing command: {} (payload: {:?})", name, payload);

        match name {
            "discord_leave_channel" => {
                tokio::task::spawn_blocking(send_ctrl_f6);
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
            "Lock" => {
                lock_workstation();
                return Ok(());
            }
            "Hibernate" => {
                hibernate();
                return Ok(());
            }
            "Restart" => {
                restart();
                return Ok(());
            }
            "volume_set" => {
                if let Ok(level) = payload.parse::<f32>() {
                    audio::set_volume(level);
                }
                return Ok(());
            }
            "volume_mute" => {
                // Button sends "PRESS" - toggle mute
                // Service call can send "true"/"false" to set specific state
                if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                    audio::toggle_mute();
                } else {
                    let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
                    audio::set_mute(mute);
                }
                return Ok(());
            }
            "volume_toggle_mute" => {
                audio::toggle_mute();
                return Ok(());
            }
            "media_play_pause" => {
                audio::send_media_key(MediaKey::PlayPause);
                return Ok(());
            }
            "media_next" => {
                audio::send_media_key(MediaKey::Next);
                return Ok(());
            }
            "media_previous" => {
                audio::send_media_key(MediaKey::Previous);
                return Ok(());
            }
            "media_stop" => {
                audio::send_media_key(MediaKey::Stop);
                return Ok(());
            }
            _ => {}
        }

        // Check for custom command first
        if execute_custom_command(state, name).await? {
            return Ok(());
        }

        // Get command string
        let cmd_str = if let Some(predefined) = get_predefined_command(name) {
            predefined.to_string()
        } else if !payload.is_empty() {
            // Launcher shortcuts (steam:, epic:, exe:, lnk:, close:) are always
            // allowed - they're validated and safe, no raw shell execution.
            if let Some(expanded) = expand_launcher_shortcut(payload) {
                expanded
            } else {
                // Raw payload execution: only allowed if configured
                let config = state.config.read().await;
                if !config.allow_raw_commands {
                    warn!("Raw command blocked (allow_raw_commands=false): {}", name);
                    return Ok(());
                }
                payload.to_string()
            }
        } else {
            warn!("No command configured for: {}", name);
            return Ok(());
        };

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
    let ps_cmdlets = [
        "Start-Process",
        "Add-Type",
        "Get-Process",
        "Stop-Process",
        "Set-",
        "Get-",
        "New-",
        "Remove-",
        "Invoke-",
    ];
    !ps_cmdlets.iter().any(|prefix| cmd.starts_with(prefix))
}

/// Expand Windows-style %VAR% environment variables (single-pass)
fn expand_env_vars(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }

    let mut result = String::with_capacity(s.len());
    let mut chars = s.char_indices();

    while let Some((i, c)) = chars.next() {
        if c == '%' {
            let var_start = i + 1;
            let mut found_end = false;
            for (j, c2) in chars.by_ref() {
                if c2 == '%' {
                    let var_name = &s[var_start..j];
                    result.push_str(&std::env::var(var_name).unwrap_or_default());
                    found_end = true;
                    break;
                }
            }
            if !found_end {
                // No closing %, keep literal
                result.push('%');
                result.push_str(&s[var_start..]);
                return result;
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Send Ctrl+F6 keypress (Discord leave channel hotkey)
fn send_ctrl_f6() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, keybd_event,
    };

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

/// Lock workstation (native, no PowerShell)
fn lock_workstation() {
    use windows::Win32::System::Shutdown::LockWorkStation;
    unsafe {
        let _ = LockWorkStation();
    }
}

/// Hibernate (native, no PowerShell)
fn hibernate() {
    use windows::Win32::System::Power::SetSuspendState;
    unsafe {
        // SetSuspendState(hibernate=true, force=false, wakeupEventsDisabled=false)
        let _ = SetSuspendState(true, false, false);
    }
}

/// Restart system (native, no PowerShell)
fn restart() {
    use windows::Win32::Foundation::{HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Shutdown::{EWX_REBOOT, ExitWindowsEx, SHUTDOWN_REASON};
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::Win32::System::Threading::OpenProcessToken;
    use windows::core::w;

    unsafe {
        // Get shutdown privilege
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &raw mut token,
        )
        .is_ok()
        {
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(None, w!("SeShutdownPrivilege"), &raw mut luid).is_ok() {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                let _ = AdjustTokenPrivileges(token, false, Some(&raw const tp), 0, None, None);
            }
        }

        // Restart
        let _ = ExitWindowsEx(EWX_REBOOT, SHUTDOWN_REASON(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- get_predefined_command tests --

    #[test]
    fn test_predefined_screensaver() {
        let cmd = get_predefined_command("Screensaver");
        assert!(cmd.is_some());
        assert!(cmd.unwrap().contains("scrnsave.scr"));
    }

    #[test]
    fn test_predefined_shutdown() {
        assert_eq!(get_predefined_command("Shutdown"), Some("shutdown -s -t 0"));
    }

    #[test]
    fn test_predefined_sleep() {
        let cmd = get_predefined_command("sleep");
        assert!(cmd.is_some());
        assert!(cmd.unwrap().contains("SetSuspendState"));
    }

    #[test]
    fn test_predefined_native_commands_return_none() {
        // These are handled natively, not via shell command
        for name in &[
            "Wake",
            "Lock",
            "Hibernate",
            "Restart",
            "volume_set",
            "volume_mute",
            "media_play_pause",
            "media_next",
            "media_previous",
            "media_stop",
        ] {
            assert!(
                get_predefined_command(name).is_none(),
                "{name} should be None"
            );
        }
    }

    #[test]
    fn test_predefined_unknown_returns_none() {
        assert!(get_predefined_command("nonexistent").is_none());
    }

    // -- needs_ampersand tests --

    #[test]
    fn test_needs_ampersand_plain_command() {
        assert!(needs_ampersand("notepad.exe"));
    }

    #[test]
    fn test_needs_ampersand_path_command() {
        assert!(needs_ampersand(r"C:\Program Files\app.exe"));
    }

    #[test]
    fn test_needs_ampersand_start_process() {
        assert!(!needs_ampersand("Start-Process notepad"));
    }

    #[test]
    fn test_needs_ampersand_get_process() {
        assert!(!needs_ampersand("Get-Process notepad"));
    }

    #[test]
    fn test_needs_ampersand_stop_process() {
        assert!(!needs_ampersand("Stop-Process -Name notepad"));
    }

    #[test]
    fn test_needs_ampersand_add_type() {
        assert!(!needs_ampersand("Add-Type -TypeDefinition ..."));
    }

    #[test]
    fn test_needs_ampersand_set_cmdlet() {
        assert!(!needs_ampersand("Set-Location C:\\"));
    }

    #[test]
    fn test_needs_ampersand_new_cmdlet() {
        assert!(!needs_ampersand("New-Item -Path test.txt"));
    }

    #[test]
    fn test_needs_ampersand_remove_cmdlet() {
        assert!(!needs_ampersand("Remove-Item test.txt"));
    }

    #[test]
    fn test_needs_ampersand_invoke_cmdlet() {
        assert!(!needs_ampersand("Invoke-WebRequest https://example.com"));
    }

    // -- expand_env_vars tests --

    #[test]
    fn test_expand_env_no_vars() {
        assert_eq!(expand_env_vars("hello world"), "hello world");
    }

    #[test]
    fn test_expand_env_known_var() {
        std::env::set_var("PC_BRIDGE_TEST_VAR", "replaced");
        assert_eq!(
            expand_env_vars("before %PC_BRIDGE_TEST_VAR% after"),
            "before replaced after"
        );
        std::env::remove_var("PC_BRIDGE_TEST_VAR");
    }

    #[test]
    fn test_expand_env_unknown_var() {
        // Unknown vars expand to empty string
        assert_eq!(
            expand_env_vars("before %UNLIKELY_VAR_39182% after"),
            "before  after"
        );
    }

    #[test]
    fn test_expand_env_unclosed_percent() {
        // Unclosed % keeps literal text
        assert_eq!(expand_env_vars("path %unclosed"), "path %unclosed");
    }

    #[test]
    fn test_expand_env_multiple_vars() {
        std::env::set_var("PC_BRIDGE_A", "X");
        std::env::set_var("PC_BRIDGE_B", "Y");
        assert_eq!(expand_env_vars("%PC_BRIDGE_A%-%PC_BRIDGE_B%"), "X-Y");
        std::env::remove_var("PC_BRIDGE_A");
        std::env::remove_var("PC_BRIDGE_B");
    }

    #[test]
    fn test_expand_env_empty_input() {
        assert_eq!(expand_env_vars(""), "");
    }
}
