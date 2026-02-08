//! Launcher shortcuts - expand short commands to full PowerShell commands
//!
//! Supported formats:
//! - steam:APPID     → launches Steam game by App ID
//! - epic:GAME       → launches Epic game by name  
//! - exe:PATH        → launches executable
//! - lnk:PATH        → launches shortcut
//! - close:NAME      → gracefully closes process

use tracing::{info, warn};

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
    let lower = arg.to_lowercase();

    if let Some(idx) = lower.find(".exe ") {
        let path = &arg[..idx + 4];
        let args = arg[idx + 5..].trim();
        return (path, if args.is_empty() { None } else { Some(args) });
    }

    if let Some(idx) = lower.find(".lnk ") {
        let path = &arg[..idx + 4];
        let args = arg[idx + 5..].trim();
        return (path, if args.is_empty() { None } else { Some(args) });
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
}
