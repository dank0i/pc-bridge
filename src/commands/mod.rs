//! Command execution module

pub mod custom;
pub mod dry_run;

use crate::config::FeatureConfig;

/// Whether the feature gating a command is currently enabled.
///
/// Destructive/native commands (Shutdown, Sleep, Lock, ...) are only registered
/// with HA when their feature is on, but the broker can still deliver a stale
/// subscription after a feature is disabled (clean_session is false and topics
/// aren't unsubscribed), so the executor must re-check here before acting.
/// Commands not tied to a feature flag (notifications, custom, raw shell) return
/// `true` and are gated by their own downstream checks.
pub(crate) fn command_feature_enabled(name: &str, f: &FeatureConfig) -> bool {
    match name {
        "Shutdown" => f.cmd_shutdown,
        "Restart" => f.cmd_restart,
        "Sleep" | "Hibernate" => f.cmd_sleep,
        "Lock" => f.cmd_lock,
        "Logoff" => f.cmd_logoff,
        "MonitorOff" | "MonitorOn" => f.cmd_monitor,
        "Launch" => f.launch_game,
        "CloseGame" => f.close_game,
        "RefreshSteamGames" => f.steam_library,
        "Screensaver" | "Wake" => f.idle_tracking,
        "DiscordJoin" | "DiscordLeaveChannel" => f.discord,
        // Audio buttons are all registered under media_controls (register_discovery);
        // volume gates the volume_level sensor, not these commands.
        "MediaPlayPause" | "MediaNext" | "MediaPrevious" | "MediaStop" => f.media_controls,
        "VolumeMute" | "VolumeSet" => f.media_controls,
        _ => true,
    }
}

/// A launch `payload` whose scheme runs an arbitrary program or URL (`exe:`,
/// `lnk:`, `url:`), as opposed to the ID/name-restricted schemes (`steam:`,
/// `epic:`, `close:`, `kill:`, `update:`, `validate:`).
pub(crate) fn is_arbitrary_launch(payload: &str) -> bool {
    payload.starts_with("exe:") || payload.starts_with("lnk:") || payload.starts_with("url:")
}

/// Whether `payload` matches a configured game's launch command (what Home
/// Assistant's Launch button publishes). Used to authorize the arbitrary-launch
/// schemes above so an attacker with MQTT access can't run an unconfigured
/// program while `allow_raw_commands` is off.
pub(crate) fn is_configured_launch(config: &crate::config::Config, payload: &str) -> bool {
    config
        .games
        .values()
        .filter_map(|g| g.launch_command())
        .any(|lc| lc == payload)
}

#[cfg(test)]
mod tests {
    use super::{command_feature_enabled, is_arbitrary_launch};
    use crate::config::FeatureConfig;

    #[test]
    fn test_is_arbitrary_launch() {
        assert!(is_arbitrary_launch("exe:C:/Games/g.exe"));
        assert!(is_arbitrary_launch("lnk:C:/x.lnk"));
        assert!(is_arbitrary_launch("url:steam://run/1"));
        // ID/name-restricted schemes are not "arbitrary".
        assert!(!is_arbitrary_launch("steam:730"));
        assert!(!is_arbitrary_launch("epic:Fortnite"));
        assert!(!is_arbitrary_launch("close:notepad"));
    }

    #[test]
    fn test_command_feature_enabled() {
        // Defaults: power flags on, game/media flags off.
        let mut f = FeatureConfig::default();
        assert!(command_feature_enabled("Shutdown", &f));
        f.cmd_shutdown = false;
        assert!(!command_feature_enabled("Shutdown", &f));
        assert!(command_feature_enabled("Restart", &f)); // independent flag, still on
        assert!(!command_feature_enabled("Launch", &f)); // launch_game off by default
        f.launch_game = true;
        assert!(command_feature_enabled("Launch", &f));
        // Commands not tied to a feature are always allowed (gated downstream).
        assert!(command_feature_enabled("notification", &f));
        assert!(command_feature_enabled("some_custom_command", &f));
    }
}

#[cfg(windows)]
mod executor;
#[cfg(windows)]
mod launcher;

#[cfg(unix)]
mod executor_linux;
#[cfg(unix)]
mod launcher_linux;

#[cfg(windows)]
pub use executor::CommandExecutor;

#[cfg(unix)]
pub use executor_linux::CommandExecutor;
