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
use crate::steam::SteamGameDiscovery;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const MAX_CONCURRENT_COMMANDS: usize = 5;

/// Predefined commands
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some(r#"%windir%\System32\scrnsave.scr /s"#),
        // These are handled natively in execute_command
        "Wake" | "Lock" | "Hibernate" | "Restart" | "VolumeSet" | "VolumeMute"
        | "MediaPlayPause" | "MediaNext" | "MediaPrevious" | "MediaStop" => None,
        "Shutdown" => Some("shutdown -s -t 0"),
        "Sleep" => Some("Rundll32.exe powrprof.dll,SetSuspendState 0,1,0"),
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
            // Discord: Leave the current voice channel by simulating a keybind
            // (default: Ctrl+F6, Discord's "Disconnect from Voice Channel").
            // Configurable via discord_keybind in userConfig.json.
            // Runs on a blocking thread because SendInput uses sleep() between
            // key-down and key-up events.
            "DiscordLeaveChannel" => {
                let keybind = state
                    .config
                    .read()
                    .await
                    .discord_keybind
                    .clone()
                    .unwrap_or_else(|| "ctrl+f6".to_string());
                tokio::task::spawn_blocking(move || send_keybind(&keybind));
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
            "VolumeSet" => {
                if let Ok(level) = payload.parse::<f32>() {
                    tokio::task::spawn_blocking(move || audio::set_volume(level));
                }
                return Ok(());
            }
            "VolumeMute" => {
                // Button sends "PRESS" - toggle mute
                // Service call can send "true"/"false" to set specific state
                if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                    tokio::task::spawn_blocking(audio::toggle_mute);
                } else {
                    let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
                    tokio::task::spawn_blocking(move || audio::set_mute(mute));
                }
                return Ok(());
            }
            "MediaPlayPause" => {
                audio::send_media_key(MediaKey::PlayPause);
                return Ok(());
            }
            "MediaNext" => {
                audio::send_media_key(MediaKey::Next);
                return Ok(());
            }
            "MediaPrevious" => {
                audio::send_media_key(MediaKey::Previous);
                return Ok(());
            }
            "MediaStop" => {
                audio::send_media_key(MediaKey::Stop);
                return Ok(());
            }
            "RefreshSteamGames" => {
                info!("Refreshing Steam game library...");
                match SteamGameDiscovery::discover_async().await {
                    Some(discovery) => {
                        let mut config = state.config.write().await;
                        match config.merge_steam_games(&discovery) {
                            Ok(added) if added > 0 => {
                                info!(
                                    "Steam refresh: added {} new games ({}ms{})",
                                    added,
                                    discovery.build_time_ms,
                                    if discovery.from_cache { ", cached" } else { "" }
                                );
                                drop(config);
                                // Notify game sensor to rebuild cached patterns
                                let _ = state.config_generation.send(());
                            }
                            Ok(_) => {
                                info!(
                                    "Steam refresh: no new games ({}ms{})",
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

        // Check for custom command first
        if execute_custom_command(state, name).await? {
            return Ok(());
        }

        // Resolve shell command from name/payload
        let allow_raw = state.config.read().await.allow_raw_commands;
        let cmd_str = match resolve_shell_command(name, payload, allow_raw) {
            ShellResolution::Predefined(cmd)
            | ShellResolution::LauncherShortcut(cmd)
            | ShellResolution::RawCommand(cmd) => cmd,
            ShellResolution::Blocked => {
                warn!("Raw command blocked (allow_raw_commands=false): {}", name);
                return Ok(());
            }
            ShellResolution::NotFound => {
                warn!("No command configured for: {}", name);
                return Ok(());
            }
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
                    // Timeout: the spawn_blocking task still owns `child` and is
                    // blocked on child.wait(). We can't reach it to kill the
                    // process, but dropping a Child on Windows does NOT kill it.
                    // Log clearly so the user knows the process is still running.
                    warn!("Command timed out after 5 minutes (process may still be running)");
                }
            }
        });

        Ok(())
    }
}

/// The full action resolved from a command name + payload.
///
/// Captures every branch of `execute_command`'s routing logic in a testable
/// enum.  `execute_command` uses this internally, and tests can exercise the
/// whole routing table without needing Windows APIs or an `AppState`.
#[derive(Debug, PartialEq)]
pub(crate) enum CommandAction {
    /// Native side-effect handled inline (Wake, Lock, media keys, etc.)
    Native(&'static str),
    /// Show a toast notification with the given text
    Notification(String),
    /// Set volume to a specific level
    VolumeSet(f32),
    /// Mute/unmute (Some = explicit, None = toggle)
    VolumeMute(Option<bool>),
    /// Run a shell command via PowerShell
    Shell(ShellResolution),
    /// No action — unknown command with empty payload, or raw blocked
    NoOp(&'static str),
}

/// Resolve the full routing for a command, purely from name + payload + config.
///
/// This is the exact same logic as `execute_command` but returns a value
/// instead of performing side effects.  Custom commands are NOT checked here
/// (they require async config access); in `execute_command` they're tried
/// before falling through to `resolve_shell_command`.
pub(crate) fn resolve_command_action(
    name: &str,
    payload: &str,
    allow_raw_commands: bool,
) -> CommandAction {
    // Normalize payload (same as execute_command)
    let payload = payload.trim();
    let payload = if payload.eq_ignore_ascii_case("PRESS") {
        ""
    } else {
        payload
    };

    // Native commands
    match name {
        "DiscordLeaveChannel" => return CommandAction::Native("DiscordLeaveChannel"),
        "Wake" => return CommandAction::Native("Wake"),
        "Lock" => return CommandAction::Native("Lock"),
        "Hibernate" => return CommandAction::Native("Hibernate"),
        "Restart" => return CommandAction::Native("Restart"),
        "notification" => {
            if payload.is_empty() {
                return CommandAction::NoOp("notification_empty");
            }
            return CommandAction::Notification(payload.to_string());
        }
        "VolumeSet" => {
            return match payload.parse::<f32>() {
                Ok(level) => CommandAction::VolumeSet(level),
                Err(_) => CommandAction::NoOp("volume_set_invalid"),
            };
        }
        "VolumeMute" => {
            if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                return CommandAction::VolumeMute(None);
            }
            let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
            return CommandAction::VolumeMute(Some(mute));
        }
        "MediaPlayPause" => return CommandAction::Native("MediaPlayPause"),
        "MediaNext" => return CommandAction::Native("MediaNext"),
        "MediaPrevious" => return CommandAction::Native("MediaPrevious"),
        "MediaStop" => return CommandAction::Native("MediaStop"),
        _ => {}
    }

    // Shell resolution (predefined → launcher → raw → blocked → not found)
    let resolution = resolve_shell_command(name, payload, allow_raw_commands);
    match &resolution {
        ShellResolution::Blocked => CommandAction::NoOp("blocked"),
        ShellResolution::NotFound => CommandAction::NoOp("not_found"),
        _ => CommandAction::Shell(resolution),
    }
}

/// Result of resolving a command name + payload into a shell command.
///
/// Separated from `execute_command` for testability — the original bug where
/// launcher shortcuts were blocked by `allow_raw_commands=false` was missed
/// because this logic was inline and untestable without a real Windows env.
#[derive(Debug, PartialEq)]
pub(crate) enum ShellResolution {
    /// Predefined command (Screensaver, Shutdown, sleep)
    Predefined(String),
    /// Validated launcher shortcut (steam:, exe:, close:, url:, etc.)
    LauncherShortcut(String),
    /// Raw payload, allowed by config
    RawCommand(String),
    /// Raw payload blocked (allow_raw_commands=false)
    Blocked,
    /// No command found for this name/payload combination
    NotFound,
}

/// Resolve a command name + payload into a shell command string.
///
/// Called after native commands (Wake, Lock, etc.) and custom commands have
/// been checked. This function is pure (no I/O) and fully unit-testable.
///
/// Order of resolution:
/// 1. Predefined commands (matched by name)
/// 2. Launcher shortcuts (matched by payload prefix like `steam:`, `close:`)
/// 3. Raw payload (gated by `allow_raw_commands`)
/// 4. Not found (empty payload, unknown name)
pub(crate) fn resolve_shell_command(
    name: &str,
    payload: &str,
    allow_raw_commands: bool,
) -> ShellResolution {
    // 1. Predefined commands always work regardless of allow_raw_commands
    if let Some(predefined) = get_predefined_command(name) {
        return ShellResolution::Predefined(predefined.to_string());
    }

    // 2. Payload-based resolution
    if !payload.is_empty() {
        // Launcher shortcuts are always allowed — they're validated and safe
        if let Some(expanded) = expand_launcher_shortcut(payload) {
            return ShellResolution::LauncherShortcut(expanded);
        }

        // 3. Raw payload: only if configured
        if allow_raw_commands {
            return ShellResolution::RawCommand(payload.to_string());
        }
        return ShellResolution::Blocked;
    }

    // 4. No payload and not a predefined command
    ShellResolution::NotFound
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

/// Send a configurable keybind (e.g. "ctrl+f6", "ctrl+shift+m").
///
/// Parses the keybind string into modifiers + key, then simulates
/// the keypresses via `SendInput`. Spaced 10ms apart to ensure
/// the OS input queue processes them in order.
fn send_keybind(keybind: &str) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VIRTUAL_KEY,
    };

    let parts: Vec<&str> = keybind.split('+').map(str::trim).collect();
    if parts.is_empty() {
        warn!("Empty keybind string");
        return;
    }

    let mut modifiers: Vec<u8> = Vec::new();
    let mut key: Option<u8> = None;

    for part in &parts {
        let lower = part.to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => modifiers.push(0x11), // VK_CONTROL
            "shift" => modifiers.push(0x10),            // VK_SHIFT
            "alt" => modifiers.push(0x12),              // VK_MENU
            "win" | "super" => modifiers.push(0x5B),    // VK_LWIN
            k => match parse_vk_code(k) {
                Some(vk) => key = Some(vk),
                None => {
                    warn!("Unknown key in keybind: {}", part);
                    return;
                }
            },
        }
    }

    let Some(vk) = key else {
        warn!("No key found in keybind: {}", keybind);
        return;
    };

    let make_input = |vk_code: u8, flags: KEYBD_EVENT_FLAGS| -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk_code as u16),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    };

    let input_size = std::mem::size_of::<INPUT>() as i32;

    unsafe {
        // Press modifiers
        for &m in &modifiers {
            SendInput(&[make_input(m, KEYBD_EVENT_FLAGS(0))], input_size);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Press key
        SendInput(&[make_input(vk, KEYBD_EVENT_FLAGS(0))], input_size);
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Release key
        SendInput(&[make_input(vk, KEYEVENTF_KEYUP)], input_size);
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Release modifiers (reverse order)
        for &m in modifiers.iter().rev() {
            SendInput(&[make_input(m, KEYEVENTF_KEYUP)], input_size);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Map a key name to a Windows virtual-key code.
fn parse_vk_code(key: &str) -> Option<u8> {
    match key {
        "f1" => Some(0x70),
        "f2" => Some(0x71),
        "f3" => Some(0x72),
        "f4" => Some(0x73),
        "f5" => Some(0x74),
        "f6" => Some(0x75),
        "f7" => Some(0x76),
        "f8" => Some(0x77),
        "f9" => Some(0x78),
        "f10" => Some(0x79),
        "f11" => Some(0x7A),
        "f12" => Some(0x7B),
        "escape" | "esc" => Some(0x1B),
        "tab" => Some(0x09),
        "space" => Some(0x20),
        "enter" | "return" => Some(0x0D),
        "backspace" => Some(0x08),
        "delete" | "del" => Some(0x2E),
        "insert" | "ins" => Some(0x2D),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" | "pgup" => Some(0x21),
        "pagedown" | "pgdn" => Some(0x22),
        "up" => Some(0x26),
        "down" => Some(0x28),
        "left" => Some(0x25),
        "right" => Some(0x27),
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphabetic() => {
            Some(k.as_bytes()[0].to_ascii_uppercase())
        }
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_digit() => Some(k.as_bytes()[0]),
        _ => None,
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

    // ===================================================================
    // resolve_shell_command tests — the core routing logic
    // ===================================================================
    //
    // These tests cover the exact bug scenario where launcher shortcuts
    // (steam:, exe:, close:, url:) were incorrectly blocked by
    // allow_raw_commands=false. The extraction of resolve_shell_command
    // ensures this class of bug is caught by unit tests going forward.

    // -- Predefined commands --

    #[test]
    fn test_resolve_predefined_screensaver() {
        let result = resolve_shell_command("Screensaver", "", false);
        assert!(
            matches!(result, ShellResolution::Predefined(ref cmd) if cmd.contains("scrnsave.scr")),
            "Screensaver should resolve to predefined command: {result:?}"
        );
    }

    #[test]
    fn test_resolve_predefined_shutdown() {
        let result = resolve_shell_command("Shutdown", "", false);
        assert_eq!(
            result,
            ShellResolution::Predefined("shutdown -s -t 0".to_string())
        );
    }

    #[test]
    fn test_resolve_predefined_sleep() {
        let result = resolve_shell_command("Sleep", "", false);
        assert!(
            matches!(result, ShellResolution::Predefined(ref cmd) if cmd.contains("SetSuspendState")),
        );
    }

    #[test]
    fn test_resolve_predefined_ignores_allow_raw_commands() {
        // Predefined commands must work regardless of allow_raw_commands
        for allow_raw in [false, true] {
            let result = resolve_shell_command("Screensaver", "", allow_raw);
            assert!(
                matches!(result, ShellResolution::Predefined(_)),
                "Predefined should resolve with allow_raw={allow_raw}: {result:?}"
            );
        }
    }

    #[test]
    fn test_resolve_predefined_with_payload_still_predefined() {
        // If name matches predefined, the predefined wins even if payload exists
        let result = resolve_shell_command("Screensaver", "some:payload", false);
        assert!(matches!(result, ShellResolution::Predefined(_)));
    }

    // -- Launcher shortcuts (THE BUG SCENARIO) --

    #[test]
    fn test_resolve_steam_shortcut_allowed_when_raw_disabled() {
        // THIS IS THE EXACT BUG: steam: shortcuts must work even with allow_raw_commands=false
        let result = resolve_shell_command("Launch", "steam:730", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("steam://rungameid/730")),
            "steam: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_close_shortcut_allowed_when_raw_disabled() {
        // close: shortcuts must work even with allow_raw_commands=false
        let result = resolve_shell_command("Launch", "close:notepad", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("CloseMainWindow")),
            "close: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_kill_shortcut_allowed_when_raw_disabled() {
        let result = resolve_shell_command("Launch", "kill:notepad", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("CloseMainWindow")),
            "kill: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_exe_shortcut_allowed_when_raw_disabled() {
        let result = resolve_shell_command("Launch", r"exe:C:\Games\Game.exe", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("Start-Process")),
            "exe: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_lnk_shortcut_allowed_when_raw_disabled() {
        let result = resolve_shell_command("Launch", r"lnk:C:\Users\user\Desktop\Game.lnk", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("Start-Process")),
            "lnk: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_url_shortcut_allowed_when_raw_disabled() {
        let result =
            resolve_shell_command("Launch", "url:discord://discord.com/channels/1/2", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("Start-Process")),
            "url: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_epic_shortcut_allowed_when_raw_disabled() {
        let result = resolve_shell_command("Launch", "epic:Fortnite", false);
        assert!(
            matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains("epicgames")),
            "epic: shortcut must work with allow_raw_commands=false: {result:?}"
        );
    }

    #[test]
    fn test_resolve_all_launcher_types_with_raw_enabled() {
        // Sanity: all shortcuts also work when raw commands ARE allowed
        let cases = vec![
            ("steam:730", "steam://rungameid"),
            ("close:notepad", "CloseMainWindow"),
            (r"exe:C:\app.exe", "Start-Process"),
            ("url:https://example.com", "Start-Process"),
            ("epic:GameName", "epicgames"),
        ];

        for (payload, expected_substr) in cases {
            let result = resolve_shell_command("Launch", payload, true);
            assert!(
                matches!(result, ShellResolution::LauncherShortcut(ref cmd) if cmd.contains(expected_substr)),
                "Launcher shortcut should work with raw=true: payload={payload}, result={result:?}"
            );
        }
    }

    // -- Raw command blocking --

    #[test]
    fn test_resolve_raw_payload_blocked_when_disabled() {
        let result = resolve_shell_command("Launch", "notepad.exe", false);
        assert_eq!(result, ShellResolution::Blocked);
    }

    #[test]
    fn test_resolve_raw_payload_allowed_when_enabled() {
        let result = resolve_shell_command("Launch", "notepad.exe", true);
        assert_eq!(
            result,
            ShellResolution::RawCommand("notepad.exe".to_string())
        );
    }

    #[test]
    fn test_resolve_raw_complex_payload_blocked() {
        // A complex PowerShell command should be blocked without allow_raw_commands
        let result = resolve_shell_command(
            "Launch",
            "Get-Process | Where-Object { $_.CPU -gt 100 }",
            false,
        );
        assert_eq!(result, ShellResolution::Blocked);
    }

    #[test]
    fn test_resolve_raw_complex_payload_allowed() {
        let payload = "Get-Process | Where-Object { $_.CPU -gt 100 }";
        let result = resolve_shell_command("Launch", payload, true);
        assert_eq!(result, ShellResolution::RawCommand(payload.to_string()));
    }

    // -- Not found --

    #[test]
    fn test_resolve_unknown_name_empty_payload() {
        let result = resolve_shell_command("unknown_command", "", false);
        assert_eq!(result, ShellResolution::NotFound);
    }

    #[test]
    fn test_resolve_unknown_name_empty_payload_raw_enabled() {
        // Even with raw commands enabled, empty payload = not found
        let result = resolve_shell_command("unknown_command", "", true);
        assert_eq!(result, ShellResolution::NotFound);
    }

    // -- Native command names should NOT match predefined --

    #[test]
    fn test_resolve_native_commands_return_not_found() {
        // Native commands (Wake, Lock, etc.) are handled before resolve_shell_command
        // is called. If they somehow reach here, they should be NotFound.
        let native_names = [
            "Wake",
            "Lock",
            "Hibernate",
            "Restart",
            "VolumeSet",
            "VolumeMute",
            "MediaPlayPause",
            "MediaNext",
            "MediaPrevious",
            "MediaStop",
        ];

        for name in native_names {
            let result = resolve_shell_command(name, "", false);
            assert_eq!(
                result,
                ShellResolution::NotFound,
                "{name} should be NotFound (handled natively before this function)"
            );
        }
    }

    // -- Edge cases --

    #[test]
    fn test_resolve_invalid_steam_id_falls_through_to_raw() {
        // steam:abc is not valid (non-numeric), expand_launcher_shortcut returns None
        // So it falls through to raw command check
        let result = resolve_shell_command("Launch", "steam:abc", false);
        assert_eq!(result, ShellResolution::Blocked);

        let result = resolve_shell_command("Launch", "steam:abc", true);
        assert_eq!(result, ShellResolution::RawCommand("steam:abc".to_string()));
    }

    #[test]
    fn test_resolve_url_without_scheme_falls_through() {
        // url:not-a-url has no :// so expand_launcher_shortcut returns None
        let result = resolve_shell_command("Launch", "url:not-a-url", false);
        assert_eq!(result, ShellResolution::Blocked);
    }

    #[test]
    fn test_resolve_shell_injection_in_url_falls_through() {
        // Shell metacharacters are rejected by expand_launcher_shortcut
        let result = resolve_shell_command("Launch", "url:discord://x;rm -rf /", false);
        assert_eq!(result, ShellResolution::Blocked);
    }

    #[test]
    fn test_resolve_close_with_injection_falls_through() {
        // Process name with shell chars rejected by is_safe_identifier
        let result = resolve_shell_command("Launch", "close:bad;name", false);
        assert_eq!(result, ShellResolution::Blocked);
    }

    #[test]
    fn test_resolve_empty_launcher_arg_falls_through() {
        // steam: with no arg -> expand_launcher_shortcut returns None
        let result = resolve_shell_command("Launch", "steam:", false);
        assert_eq!(result, ShellResolution::Blocked);
    }

    // ===================================================================
    // get_predefined_command tests
    // ===================================================================

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
        let cmd = get_predefined_command("Sleep");
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
            "VolumeSet",
            "VolumeMute",
            "MediaPlayPause",
            "MediaNext",
            "MediaPrevious",
            "MediaStop",
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

    // ===================================================================
    // needs_ampersand tests
    // ===================================================================

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

    // ===================================================================
    // expand_env_vars tests
    // ===================================================================

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

    // ===================================================================
    // resolve_command_action tests — full end-to-end routing
    // ===================================================================
    //
    // These test the COMPLETE command pipeline: name + payload → action.
    // Covers native commands, shell resolution, payload normalisation,
    // and the critical interaction between launcher shortcuts and
    // allow_raw_commands.

    // -- Native command routing --

    #[test]
    fn test_action_wake() {
        assert_eq!(
            resolve_command_action("Wake", "", false),
            CommandAction::Native("Wake")
        );
    }

    #[test]
    fn test_action_lock() {
        assert_eq!(
            resolve_command_action("Lock", "", false),
            CommandAction::Native("Lock")
        );
    }

    #[test]
    fn test_action_hibernate() {
        assert_eq!(
            resolve_command_action("Hibernate", "", false),
            CommandAction::Native("Hibernate")
        );
    }

    #[test]
    fn test_action_restart() {
        assert_eq!(
            resolve_command_action("Restart", "", false),
            CommandAction::Native("Restart")
        );
    }

    #[test]
    fn test_action_discord_leave() {
        assert_eq!(
            resolve_command_action("DiscordLeaveChannel", "", false),
            CommandAction::Native("DiscordLeaveChannel")
        );
    }

    #[test]
    fn test_action_media_keys() {
        for name in &["MediaPlayPause", "MediaNext", "MediaPrevious", "MediaStop"] {
            assert_eq!(
                resolve_command_action(name, "PRESS", false),
                CommandAction::Native(name),
                "media key {name} should route to Native"
            );
        }
    }

    // -- Notification --

    #[test]
    fn test_action_notification_with_text() {
        assert_eq!(
            resolve_command_action("notification", "Hello world", false),
            CommandAction::Notification("Hello world".to_string())
        );
    }

    #[test]
    fn test_action_notification_empty_is_noop() {
        assert_eq!(
            resolve_command_action("notification", "", false),
            CommandAction::NoOp("notification_empty")
        );
    }

    #[test]
    fn test_action_notification_press_is_noop() {
        // "PRESS" normalises to empty
        assert_eq!(
            resolve_command_action("notification", "PRESS", false),
            CommandAction::NoOp("notification_empty")
        );
    }

    // -- Volume --

    #[test]
    fn test_action_volume_set() {
        assert_eq!(
            resolve_command_action("VolumeSet", "75", false),
            CommandAction::VolumeSet(75.0)
        );
    }

    #[test]
    fn test_action_volume_set_decimal() {
        assert_eq!(
            resolve_command_action("VolumeSet", "33.5", false),
            CommandAction::VolumeSet(33.5)
        );
    }

    #[test]
    fn test_action_volume_set_invalid() {
        assert_eq!(
            resolve_command_action("VolumeSet", "loud", false),
            CommandAction::NoOp("volume_set_invalid")
        );
    }

    #[test]
    fn test_action_volume_set_press_is_invalid() {
        // "PRESS" normalises to "" which fails to parse as f32
        assert_eq!(
            resolve_command_action("VolumeSet", "PRESS", false),
            CommandAction::NoOp("volume_set_invalid")
        );
    }

    #[test]
    fn test_action_volume_mute_toggle() {
        assert_eq!(
            resolve_command_action("VolumeMute", "", false),
            CommandAction::VolumeMute(None)
        );
    }

    #[test]
    fn test_action_volume_mute_press_toggles() {
        // "PRESS" normalises to "" → toggle
        assert_eq!(
            resolve_command_action("VolumeMute", "PRESS", false),
            CommandAction::VolumeMute(None)
        );
    }

    #[test]
    fn test_action_volume_mute_explicit_true() {
        assert_eq!(
            resolve_command_action("VolumeMute", "true", false),
            CommandAction::VolumeMute(Some(true))
        );
    }

    #[test]
    fn test_action_volume_mute_explicit_false() {
        assert_eq!(
            resolve_command_action("VolumeMute", "false", false),
            CommandAction::VolumeMute(Some(false))
        );
    }

    #[test]
    fn test_action_volume_mute_one() {
        assert_eq!(
            resolve_command_action("VolumeMute", "1", false),
            CommandAction::VolumeMute(Some(true))
        );
    }

    // -- Shell commands through the full pipeline --

    #[test]
    fn test_action_screensaver_produces_shell() {
        let action = resolve_command_action("Screensaver", "", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::Predefined(ref cmd))
                if cmd.contains("scrnsave.scr")
            ),
            "Screensaver should produce a shell command: {action:?}"
        );
    }

    #[test]
    fn test_action_shutdown_produces_shell() {
        let action = resolve_command_action("Shutdown", "", false);
        assert_eq!(
            action,
            CommandAction::Shell(ShellResolution::Predefined("shutdown -s -t 0".to_string()))
        );
    }

    #[test]
    fn test_action_sleep_produces_shell() {
        let action = resolve_command_action("Sleep", "", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::Predefined(ref cmd))
                if cmd.contains("SetSuspendState")
            ),
            "Sleep should produce shell command: {action:?}"
        );
    }

    // -- THE CRITICAL BUG TEST --
    // Launcher shortcuts (steam:, exe:, close:, url:) must work even
    // when allow_raw_commands=false. This was the exact bug in v2.12.0.

    #[test]
    fn test_action_launch_steam_raw_disabled() {
        let action = resolve_command_action("Launch", "steam:730", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::LauncherShortcut(ref cmd))
                if cmd.contains("steam://rungameid/730")
            ),
            "steam: launcher MUST work with raw=false: {action:?}"
        );
    }

    #[test]
    fn test_action_launch_close_raw_disabled() {
        let action = resolve_command_action("Launch", "close:notepad", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::LauncherShortcut(ref cmd))
                if cmd.contains("CloseMainWindow")
            ),
            "close: launcher MUST work with raw=false: {action:?}"
        );
    }

    #[test]
    fn test_action_launch_exe_raw_disabled() {
        let action = resolve_command_action("Launch", r"exe:C:\Games\Game.exe", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::LauncherShortcut(ref cmd))
                if cmd.contains("Start-Process")
            ),
            "exe: launcher MUST work with raw=false: {action:?}"
        );
    }

    #[test]
    fn test_action_launch_url_raw_disabled() {
        let action =
            resolve_command_action("Launch", "url:discord://discord.com/channels/123", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::LauncherShortcut(ref cmd))
                if cmd.contains("discord://")
            ),
            "url: launcher MUST work with raw=false: {action:?}"
        );
    }

    #[test]
    fn test_action_launch_epic_raw_disabled() {
        let action = resolve_command_action("Launch", "epic:Fortnite", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::LauncherShortcut(ref cmd))
                if cmd.contains("epicgames")
            ),
            "epic: launcher MUST work with raw=false: {action:?}"
        );
    }

    // -- Raw command blocking (full pipeline) --

    #[test]
    fn test_action_raw_command_blocked() {
        assert_eq!(
            resolve_command_action("Launch", "notepad.exe", false),
            CommandAction::NoOp("blocked")
        );
    }

    #[test]
    fn test_action_raw_command_allowed() {
        let action = resolve_command_action("Launch", "notepad.exe", true);
        assert_eq!(
            action,
            CommandAction::Shell(ShellResolution::RawCommand("notepad.exe".to_string()))
        );
    }

    // -- Unknown command --

    #[test]
    fn test_action_unknown_empty_payload() {
        assert_eq!(
            resolve_command_action("totally_unknown", "", false),
            CommandAction::NoOp("not_found")
        );
    }

    // -- PRESS normalisation --

    #[test]
    fn test_action_native_with_press_payload() {
        // "PRESS" should be normalised to "" for native commands
        assert_eq!(
            resolve_command_action("Wake", "PRESS", false),
            CommandAction::Native("Wake")
        );
    }

    #[test]
    fn test_action_press_normalisation_case_insensitive() {
        assert_eq!(
            resolve_command_action("Lock", "press", false),
            CommandAction::Native("Lock")
        );
        assert_eq!(
            resolve_command_action("Lock", "Press", false),
            CommandAction::Native("Lock")
        );
    }

    // -- Payload trimming --

    #[test]
    fn test_action_payload_whitespace_trimmed() {
        let action = resolve_command_action("Launch", "  steam:730  ", false);
        assert!(
            matches!(
                action,
                CommandAction::Shell(ShellResolution::LauncherShortcut(ref cmd))
                if cmd.contains("steam://rungameid/730")
            ),
            "Whitespace-padded payload should still resolve: {action:?}"
        );
    }

    // -- Native commands ignore payload --

    #[test]
    fn test_action_wake_ignores_extra_payload() {
        assert_eq!(
            resolve_command_action("Wake", "some extra data", false),
            CommandAction::Native("Wake")
        );
    }

    // -- Shell injection through full pipeline --

    #[test]
    fn test_action_url_injection_blocked() {
        // Shell metacharacters in URL → launcher returns None → blocked
        assert_eq!(
            resolve_command_action("Launch", "url:discord://x;rm -rf /", false),
            CommandAction::NoOp("blocked")
        );
    }

    #[test]
    fn test_action_close_injection_blocked() {
        assert_eq!(
            resolve_command_action("Launch", "close:bad;name", false),
            CommandAction::NoOp("blocked")
        );
    }
}
