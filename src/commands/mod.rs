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
        "VolumeMute" => f.media_controls,
        _ => true,
    }
}

/// True if `name` is a built-in command. Custom commands must not reuse these:
/// a same-named custom command registers to the identical retained config +
/// action topic, so the native executor claims the button press and the user's
/// script never runs.
pub(crate) fn is_native_command(name: &str) -> bool {
    matches!(
        name,
        "Shutdown"
            | "Restart"
            | "Sleep"
            | "Hibernate"
            | "Lock"
            | "Logoff"
            | "MonitorOff"
            | "MonitorOn"
            | "Launch"
            | "CloseGame"
            | "RefreshSteamGames"
            | "Screensaver"
            | "Wake"
            | "DiscordJoin"
            | "DiscordLeaveChannel"
            | "MediaPlayPause"
            | "MediaNext"
            | "MediaPrevious"
            | "MediaStop"
            | "VolumeMute"
    )
}

/// A launch `payload` whose scheme runs an arbitrary program or URL (`exe:`,
/// `lnk:`, `url:`), as opposed to the ID/name-restricted schemes (`steam:`,
/// `epic:`, `close:`, `kill:`, `update:`, `validate:`).
///
/// The scheme is extracted EXACTLY as `expand_launcher_shortcut` does (split at
/// the first ':', trim, lowercase), so `EXE:`, `exe :`, etc. can't slip past the
/// gate while still resolving to a launch. Callers must pass the same string the
/// resolver consumes (on Windows that is the env-expanded payload).
pub(crate) fn is_arbitrary_launch(payload: &str) -> bool {
    match payload.split_once(':') {
        Some((scheme, rest)) => match scheme.trim().to_ascii_lowercase().as_str() {
            "exe" | "lnk" => true,
            // url:discord://... is the DiscordJoin channel deep-link (a feature
            // gated by f.discord), not an arbitrary program/URL launch. Any other
            // url: target is arbitrary. Metacharacters are still validated by
            // is_safe_url downstream regardless.
            "url" => !rest
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("discord://"),
            _ => false,
        },
        None => false,
    }
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

/// Global launch/close authorization gate. Returns true if this payload should be
/// BLOCKED because it targets something outside the configured games list and the
/// corresponding global permission is off:
/// - launch schemes (steam/epic/update/validate) need `allow_global_launch`
///   (default ON) to reach an unconfigured title;
/// - close/kill need `allow_global_close` (default OFF) to reach a process that
///   isn't a configured game.
///
/// Other schemes (exe/lnk/url) return false here - they're governed by the
/// separate `allow_raw_commands` / arbitrary-launch gate. Pass the same payload
/// the resolver consumes (env-expanded on Windows).
pub(crate) fn global_scheme_blocked(config: &crate::config::Config, payload: &str) -> bool {
    let Some((scheme, target)) = payload.split_once(':') else {
        return false;
    };
    match scheme.trim().to_ascii_lowercase().as_str() {
        "steam" | "epic" | "update" | "validate" => {
            !config.allow_global_launch && !is_configured_launch(config, payload)
        }
        "close" | "kill" => {
            let target = target.trim().trim_end_matches(".exe");
            !config.allow_global_close && config.matching_game_processes([target]).is_empty()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{command_feature_enabled, global_scheme_blocked, is_arbitrary_launch};
    use crate::config::FeatureConfig;

    #[test]
    fn test_global_scheme_gate_defaults() {
        // Defaults: global launch ON, global close OFF, no configured games.
        let mut cfg = crate::config::Config::default();
        assert!(cfg.allow_global_launch && !cfg.allow_global_close);

        // Launch an unconfigured title: allowed by default.
        assert!(!global_scheme_blocked(&cfg, "steam:730"));
        assert!(!global_scheme_blocked(&cfg, "epic:Fortnite"));
        // Close/kill an unconfigured process: blocked by default.
        assert!(global_scheme_blocked(&cfg, "kill:notepad"));
        assert!(global_scheme_blocked(&cfg, "close:chrome.exe"));
        // exe/lnk/url are governed by allow_raw_commands, not this gate.
        assert!(!global_scheme_blocked(&cfg, "exe:/usr/bin/x"));

        // Flip the permissions.
        cfg.allow_global_launch = false;
        assert!(global_scheme_blocked(&cfg, "steam:730")); // now blocked
        cfg.allow_global_close = true;
        assert!(!global_scheme_blocked(&cfg, "kill:notepad")); // now allowed
    }

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
    fn test_is_arbitrary_launch_normalizes_scheme() {
        // The resolver lowercases + trims the scheme, so the gate must too, or
        // these would slip past while still resolving to a launch.
        assert!(is_arbitrary_launch("EXE:C:/x.exe"));
        assert!(is_arbitrary_launch("Exe:C:/x.exe"));
        assert!(is_arbitrary_launch("exe :C:/x.exe"));
        assert!(is_arbitrary_launch("  URL:steam://run/1"));
        assert!(is_arbitrary_launch("LNK:C:/x.lnk"));
    }

    #[test]
    fn test_discord_deeplink_not_arbitrary() {
        // DiscordJoin's channel deep-link is feature-gated, not arbitrary exec.
        assert!(!is_arbitrary_launch(
            "url:discord://discord.com/channels/1/2"
        ));
        assert!(!is_arbitrary_launch("URL:discord://x"));
        // Any other url: target is still arbitrary.
        assert!(is_arbitrary_launch("url:file:///etc/passwd"));
        assert!(is_arbitrary_launch("url:https://evil"));
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
