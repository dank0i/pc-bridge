//! Shared, platform-independent dry-run resolution.
//!
//! With `--dry-run`, every command is short-circuited here: resolved to a
//! canonical action string and reported to the test topic, with no OS side
//! effect. Both the Windows and Linux executors call [`report`] at the top of
//! `execute_command`, so the integration test kit sees identical results on any
//! host. The action is deliberately platform-neutral (routing identity, not the
//! platform shell string) - expansion correctness is covered by the launcher
//! unit tests, and the real launch path by the test kit's live launch self-test.

use std::sync::Arc;

use log::info;

use crate::AppState;

/// Known launcher-shortcut schemes (the `Launch` command carries one as payload).
const LAUNCHER_SCHEMES: &[&str] = &[
    "steam", "update", "validate", "epic", "exe", "lnk", "url", "close", "kill",
];

/// Resolve, log, and report a command to the test topic without performing it.
pub async fn report(name: &str, payload: &str, state: &Arc<AppState>) {
    let action = resolve_action(name, payload, state).await;
    info!("[dry-run] {name} -> {action}");
    state.mqtt.publish_test_action(name, payload, &action).await;
}

/// Map a command name + (normalized) payload to a canonical, platform-neutral
/// action. The single source of truth the test kit asserts against.
async fn resolve_action(name: &str, payload: &str, state: &Arc<AppState>) -> String {
    match name {
        "DiscordLeaveChannel" => {
            let keybind = state
                .config
                .read()
                .await
                .discord_keybind
                .clone()
                .unwrap_or_else(|| "ctrl+f6".to_string());
            format!("keybind:{keybind}")
        }
        "Wake" => "native:wake".to_string(),
        "Lock" => "native:lock".to_string(),
        "Shutdown" => "native:shutdown".to_string(),
        "Sleep" => "native:sleep".to_string(),
        "Hibernate" => "native:hibernate".to_string(),
        "Restart" => "native:restart".to_string(),
        "Logoff" => "native:logoff".to_string(),
        "MonitorOff" => "native:monitor_off".to_string(),
        "MonitorOn" => "native:monitor_on".to_string(),
        "CloseGame" => "native:close_game".to_string(),
        "Screensaver" => "native:screensaver".to_string(),
        "RefreshSteamGames" => "native:refresh_steam_games".to_string(),
        "MediaPlayPause" => "media:play_pause".to_string(),
        "MediaNext" => "media:next".to_string(),
        "MediaPrevious" => "media:previous".to_string(),
        "MediaStop" => "media:stop".to_string(),
        "VolumeSet" => format!("volume:set:{payload}"),
        "VolumeMute" => {
            if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                "volume:mute_toggle".to_string()
            } else {
                let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
                format!("volume:mute:{mute}")
            }
        }
        "notification" => format!("notification:{payload}"),
        _ => {
            // Config-defined custom command takes priority over shell resolution,
            // matching execute_command (which checks custom commands first).
            {
                let config = state.config.read().await;
                if config.custom_commands_enabled
                    && config.custom_commands.iter().any(|c| c.name == name)
                {
                    return format!("custom:{name}");
                }
            }
            // Launcher shortcut carried in the payload (the Launch command).
            if let Some((scheme, _)) = payload.split_once(':')
                && LAUNCHER_SCHEMES
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(scheme.trim()))
            {
                return format!("launch:{payload}");
            }
            if payload.is_empty() {
                return "not_found".to_string();
            }
            if state.config.read().await.allow_raw_commands {
                format!("raw:{payload}")
            } else {
                "blocked".to_string()
            }
        }
    }
}
