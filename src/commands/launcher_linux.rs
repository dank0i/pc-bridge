//! Launcher shortcuts for Linux - expand short commands to shell commands
//!
//! Supported formats:
//! - steam:APPID     - launches Steam game by App ID via xdg-open
//! - epic:GAME       - launches Epic game by name (Heroic launcher)
//! - exe:PATH        - launches executable directly
//! - url:URL         - opens a protocol URL via xdg-open
//! - close:NAME      - gracefully closes process via SIGTERM

use log::{info, warn};

/// Expand a launcher shortcut to a shell command.
/// Returns None if not a launcher shortcut.
pub fn expand_launcher_shortcut(cmd: &str) -> Option<String> {
    let (launcher, arg) = cmd.split_once(':')?;
    let launcher = launcher.trim().to_lowercase();
    let arg = arg.trim();

    if arg.is_empty() {
        return None;
    }

    match launcher.as_str() {
        "steam" => {
            if !is_numeric(arg) {
                warn!("Invalid Steam App ID (must be numeric): {}", arg);
                return None;
            }
            info!("Launching Steam game: App ID {}", arg);
            Some(format!("xdg-open 'steam://rungameid/{}'", arg))
        }

        "epic" => {
            if !is_safe_identifier(arg) {
                warn!("Invalid Epic/Heroic game name: {}", arg);
                return None;
            }
            info!("Launching Epic/Heroic game: {}", arg);
            // Heroic Games Launcher uses heroic:// protocol on Linux
            Some(format!("xdg-open 'heroic://launch/{}'", arg))
        }

        "exe" => {
            if !is_safe_path(arg) {
                warn!("Invalid path (contains shell metacharacters): {}", arg);
                return None;
            }
            info!("Launching: {}", arg);

            let (exe_path, exe_args) = split_exe_args(arg);

            if let Some(args) = exe_args {
                Some(format!("'{}' {}", exe_path, args))
            } else {
                Some(format!("'{}'", exe_path))
            }
        }

        "url" => {
            if !is_safe_url(arg) {
                warn!(
                    "Invalid URL (must be protocol://path with safe characters): {}",
                    arg
                );
                return None;
            }
            info!("Opening URL: {}", arg);
            Some(format!("xdg-open '{}'", arg))
        }

        "close" | "kill" => {
            let process_name = arg.trim_end_matches(".exe");
            if !is_safe_identifier(process_name) {
                warn!("Invalid process name: {}", arg);
                return None;
            }
            info!("Closing process: {}", arg);
            // SIGTERM for graceful shutdown, matching Windows CloseMainWindow behavior
            Some(format!("pkill -f '{}'", process_name))
        }

        // lnk: is Windows-only (.lnk shortcuts), skip on Linux
        "lnk" => {
            warn!("lnk: shortcuts are Windows-only, ignoring: {}", arg);
            None
        }

        _ => None,
    }
}

/// Split executable path from arguments.
/// e.g., "/opt/games/game --fullscreen" -> ("/opt/games/game", Some("--fullscreen"))
fn split_exe_args(arg: &str) -> (&str, Option<&str>) {
    // On Linux, executables don't have mandatory extensions.
    // Split on first space that isn't inside a quoted path.
    // Simple heuristic: if the path starts with quotes, find closing quote.
    if arg.starts_with('\'') || arg.starts_with('"') {
        let quote = arg.as_bytes()[0];
        if let Some(end) = arg[1..].find(quote as char) {
            let path = &arg[1..=end];
            let rest = arg[end + 2..].trim();
            return (path, if rest.is_empty() { None } else { Some(rest) });
        }
    }

    // Otherwise split on first space
    if let Some(pos) = arg.find(' ') {
        let path = &arg[..pos];
        let args = arg[pos + 1..].trim();
        (path, if args.is_empty() { None } else { Some(args) })
    } else {
        (arg, None)
    }
}

/// Check if string contains only digits
fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Check if string is a safe identifier (alphanumeric with .-_)
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Check if path doesn't contain dangerous shell metacharacters
fn is_safe_path(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '$' | '`' | '\n' | '\r'))
}

/// Check if string is a safe protocol URL (scheme://path, no shell metacharacters)
fn is_safe_url(s: &str) -> bool {
    let Some((scheme, _rest)) = s.split_once("://") else {
        return false;
    };
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
    {
        return false;
    }
    !s.chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '$' | '`' | '\'' | '\n' | '\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_steam_shortcut() {
        let result = expand_launcher_shortcut("steam:1234");
        assert_eq!(
            result,
            Some("xdg-open 'steam://rungameid/1234'".to_string())
        );
    }

    #[test]
    fn test_steam_rejects_non_numeric() {
        assert_eq!(expand_launcher_shortcut("steam:abc"), None);
    }

    #[test]
    fn test_exe_shortcut() {
        let result = expand_launcher_shortcut("exe:/opt/games/game");
        assert_eq!(result, Some("'/opt/games/game'".to_string()));
    }

    #[test]
    fn test_exe_with_args() {
        let result = expand_launcher_shortcut("exe:/opt/games/game --fullscreen");
        assert_eq!(result, Some("'/opt/games/game' --fullscreen".to_string()));
    }

    #[test]
    fn test_close_shortcut() {
        let result = expand_launcher_shortcut("close:firefox");
        assert_eq!(result, Some("pkill -f 'firefox'".to_string()));
    }

    #[test]
    fn test_close_strips_exe_extension() {
        let result = expand_launcher_shortcut("close:notepad.exe");
        assert_eq!(result, Some("pkill -f 'notepad'".to_string()));
    }

    #[test]
    fn test_url_shortcut() {
        let result = expand_launcher_shortcut("url:https://example.com");
        assert_eq!(result, Some("xdg-open 'https://example.com'".to_string()));
    }

    #[test]
    fn test_url_rejects_no_scheme() {
        assert_eq!(expand_launcher_shortcut("url:not-a-url"), None);
    }

    #[test]
    fn test_url_rejects_shell_injection() {
        assert_eq!(expand_launcher_shortcut("url:https://x;rm -rf /"), None);
        assert_eq!(expand_launcher_shortcut("url:https://x|evil"), None);
        assert_eq!(expand_launcher_shortcut("url:https://x&evil"), None);
    }

    #[test]
    fn test_lnk_rejected_on_linux() {
        assert_eq!(expand_launcher_shortcut("lnk:C:\\shortcut.lnk"), None);
    }

    #[test]
    fn test_epic_shortcut() {
        let result = expand_launcher_shortcut("epic:Fortnite");
        assert_eq!(
            result,
            Some("xdg-open 'heroic://launch/Fortnite'".to_string())
        );
    }

    #[test]
    fn test_empty_arg_rejected() {
        assert_eq!(expand_launcher_shortcut("steam:"), None);
        assert_eq!(expand_launcher_shortcut("exe:"), None);
    }
}
