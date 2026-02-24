//! Launcher shortcuts - expand short commands to full PowerShell commands
//!
//! Supported formats:
//! - steam:APPID     → launches Steam game by App ID
//! - epic:GAME       → launches Epic game by name  
//! - exe:PATH        → launches executable
//! - lnk:PATH        → launches shortcut
//! - url:URL         → opens a protocol URL (discord://, https://, etc.)
//! - close:NAME      → gracefully closes process

use log::{info, warn};

/// Expand a launcher shortcut to a full PowerShell command.
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
            Some(format!(r#"Start-Process "steam://rungameid/{}""#, arg))
        }

        "epic" => {
            if !is_safe_identifier(arg) {
                warn!("Invalid Epic game name: {}", arg);
                return None;
            }
            info!("Launching Epic game: {}", arg);
            Some(format!(
                r#"Start-Process "com.epicgames.launcher://apps/{}?action=launch&silent=true""#,
                arg
            ))
        }

        "exe" | "lnk" => {
            if !is_safe_path(arg) {
                warn!("Invalid path (contains shell metacharacters): {}", arg);
                return None;
            }
            info!("Launching: {}", arg);

            // Split path and arguments at .exe or .lnk
            let (exe_path, exe_args) = split_exe_args(arg);

            // Quote path if it contains spaces
            let exe_path = if exe_path.contains(' ') && !exe_path.starts_with('"') {
                format!(r#""{}""#, exe_path)
            } else {
                exe_path.to_string()
            };

            if let Some(args) = exe_args {
                Some(format!(
                    r#"Start-Process {} -ArgumentList '{}'"#,
                    exe_path, args
                ))
            } else {
                Some(format!(r#"Start-Process {}"#, exe_path))
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
            Some(format!(r#"Start-Process "{}""#, arg))
        }

        "close" | "kill" => {
            let process_name = arg.trim_end_matches(".exe");
            if !is_safe_identifier(process_name) {
                warn!("Invalid process name: {}", arg);
                return None;
            }
            info!("Closing process: {}", arg);
            Some(format!(
                r#"Get-Process | Where-Object {{ $_.ProcessName -eq '{}' }} | ForEach-Object {{ $_.CloseMainWindow() }}"#,
                process_name
            ))
        }

        _ => None,
    }
}

/// Split executable path from arguments
/// e.g., "C:\Games\Game.exe -fullscreen" → ("C:\Games\Game.exe", Some("-fullscreen"))
fn split_exe_args(arg: &str) -> (&str, Option<&str>) {
    // Zero-alloc: byte-level sliding window for case-insensitive ".exe " / ".lnk " search
    for pattern in [b".exe " as &[u8], b".lnk "] {
        if let Some(pos) = arg
            .as_bytes()
            .windows(pattern.len())
            .position(|w| w.eq_ignore_ascii_case(pattern))
        {
            let ext_len = pattern.len() - 1; // exclude trailing space
            let path = &arg[..pos + ext_len];
            let args = arg[pos + pattern.len()..].trim();
            return (path, if args.is_empty() { None } else { Some(args) });
        }
    }

    (arg, None)
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
            .any(|c| matches!(c, ';' | '|' | '&' | '$' | '`' | '"' | '\'' | '\n' | '\r'))
}

/// Check if string is a safe protocol URL (scheme://path, no shell metacharacters)
fn is_safe_url(s: &str) -> bool {
    // Must have a scheme (e.g., discord://, https://)
    let Some((scheme, _rest)) = s.split_once("://") else {
        return false;
    };
    // Scheme must be alphanumeric/dots/hyphens (e.g., "com.epicgames.launcher")
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
    {
        return false;
    }
    // No shell metacharacters in the full URL
    !s.chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '$' | '`' | '\'' | '\n' | '\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_steam_shortcut() {
        assert_eq!(
            expand_launcher_shortcut("steam:1234"),
            Some(r#"Start-Process "steam://rungameid/1234""#.to_string())
        );
        assert_eq!(expand_launcher_shortcut("steam:abc"), None);
    }

    #[test]
    fn test_exe_shortcut() {
        let result = expand_launcher_shortcut(r"exe:C:\Games\Game.exe");
        assert!(result.is_some());
        assert!(result.unwrap().contains("Start-Process"));
    }

    #[test]
    fn test_close_shortcut() {
        let result = expand_launcher_shortcut("close:notepad");
        assert!(result.is_some());
        assert!(result.unwrap().contains("CloseMainWindow"));
    }

    #[test]
    fn test_url_shortcut_discord() {
        let result = expand_launcher_shortcut("url:discord://discord.com/channels/123/456");
        assert_eq!(
            result,
            Some(r#"Start-Process "discord://discord.com/channels/123/456""#.to_string())
        );
    }

    #[test]
    fn test_url_shortcut_https() {
        let result = expand_launcher_shortcut("url:https://example.com");
        assert!(result.is_some());
        assert!(result.unwrap().contains("Start-Process"));
    }

    #[test]
    fn test_url_shortcut_rejects_no_scheme() {
        assert_eq!(expand_launcher_shortcut("url:not-a-url"), None);
    }

    #[test]
    fn test_url_shortcut_rejects_shell_injection() {
        assert_eq!(expand_launcher_shortcut("url:discord://x;rm -rf /"), None);
        assert_eq!(expand_launcher_shortcut("url:discord://x|evil"), None);
        assert_eq!(expand_launcher_shortcut("url:discord://x&evil"), None);
    }
}
