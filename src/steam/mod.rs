//! Steam game auto-discovery
//!
//! Parses Steam's VDF files to auto-discover installed games and their executables.
//! Uses memory-mapped I/O and cached indexing for minimal overhead.

mod appinfo;
pub(crate) mod discovery;
pub(crate) mod vdf;

use std::path::PathBuf;

pub use discovery::SteamGameDiscovery;

/// True if an executable name looks like a launcher, anti-cheat wrapper,
/// installer, or other non-game helper that we should not monitor as the
/// game's own process.
///
/// Accepts a bare name or a full path (`start_protected_game.exe`,
/// `EasyAntiCheat/EACLauncher.exe`); matching is case-insensitive and ignores
/// the directory and `.exe` suffix. Shared by appinfo launch-config selection
/// and the on-disk folder scan so both agree on what "not the game" means.
pub(crate) fn is_non_game_exe(exe: &str) -> bool {
    /// Helper markers matched as WHOLE tokens (equality), not substrings, so a
    /// game whose name merely contains one of these letters-runs isn't excluded:
    /// "Observer" is not a "server", "Crashlands" is not a "crash". A helper exe
    /// exposes the marker as a camelCase/separator-bounded token
    /// (`PalServer` -> pal|server, `CrashReporter` -> crash|reporter).
    const TOKENS: &[&str] = &[
        "crash",
        "report",
        "reporter",
        "launcher",
        "setup",
        "install",
        "installer",
        "uninstall",
        "uninstaller",
        "update",
        "updater",
        "helper",
        "capture",
        "message",
        "console",
        "diagnostic",
        "diagnostics",
        "upload",
        "uploader",
        "profile",
        "profiler",
        "protected",
        "server",
        "dedicated",
    ];
    /// Compound markers with no internal boundary that tokenization can't split,
    /// so they stay substring rules (`unins000`, `EasyAntiCheat`, `vconsole`).
    const SUBSTR: &[&str] = &[
        "unins",
        "redis",
        "anticheat",
        "easyanticheat",
        "battleye",
        "systeminfo",
        "vconsole",
        "crashhandler",
        "unitycrashhandler",
    ];
    /// Prefixes that mark an exe as a redistributable/launcher.
    const STARTS_WITH: &[&str] = &[
        "vc_", "vcredist", "dotnet", "directx", "dxsetup", "physx", "uplay", "ubi", "client_",
        "start_",
    ];

    let file = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
    // Strip a case-insensitive .exe but keep original case for camelCase tokens.
    // The shared helper, which uses `> 4` so a bare ".exe" is not collapsed to "".
    let stem = crate::commands::strip_exe(file);
    let lower = stem.to_lowercase();
    let tokens = tokenize(stem);

    TOKENS.iter().any(|t| tokens.iter().any(|tok| tok == t))
        || SUBSTR.iter().any(|s| lower.contains(s))
        || STARTS_WITH.iter().any(|p| lower.starts_with(p))
}

/// Split a name into lowercased alphanumeric tokens, breaking on non-alphanumeric
/// separators AND camelCase boundaries (lower/digit -> Upper, and the acronym end
/// Upper -> Upper-followed-by-lower, so `EACLauncher` -> eac|launcher). Shared by
/// the non-game filter and the discovery tie-break penalty so both match on word
/// boundaries instead of raw substrings.
pub(crate) fn tokenize(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Whether any of `name`'s tokens equals one of `markers`. Word-boundary variant
/// of a substring check (so "Protest" is not a "test", "Demolition" not a "demo").
pub(crate) fn has_marker_token(name: &str, markers: &[&str]) -> bool {
    let tokens = tokenize(name);
    markers.iter().any(|m| tokens.iter().any(|t| t == m))
}

/// Find Steam installation path (shared across modules).
///
/// Checks (in order): HKCU registry, HKLM registry, common paths (Windows)
/// or STEAM_DIR env var, then standard Linux paths (Unix).
#[cfg(windows)]
pub fn find_steam_path() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    // Try HKCU first (current user), then HKLM (all users)
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(steam_key) = hkcu.open_subkey("Software\\Valve\\Steam")
        && let Ok(path) = steam_key.get_value::<String, _>("SteamPath")
    {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for subkey in [
        "SOFTWARE\\WOW6432Node\\Valve\\Steam",
        "SOFTWARE\\Valve\\Steam",
    ] {
        if let Ok(steam_key) = hklm.open_subkey(subkey)
            && let Ok(path) = steam_key.get_value::<String, _>("InstallPath")
        {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }
    }

    // Fallback to common paths
    let common_paths = [
        "C:\\Program Files (x86)\\Steam",
        "C:\\Program Files\\Steam",
        "D:\\Steam",
        "D:\\SteamLibrary",
    ];
    for path in common_paths {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    None
}

#[cfg(unix)]
pub fn find_steam_path() -> Option<PathBuf> {
    // Check STEAM_DIR env var first (custom installs)
    if let Ok(dir) = std::env::var("STEAM_DIR") {
        let p = PathBuf::from(dir);
        if p.join("steamapps").is_dir() {
            return Some(p);
        }
    }

    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let candidates = [
        home.join(".steam/steam"),
        home.join(".local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        home.join("snap/steam/common/.local/share/Steam"),
    ];
    candidates
        .into_iter()
        .find(|p| p.join("steamapps").is_dir())
}

#[cfg(test)]
mod tests {
    use super::{is_non_game_exe, tokenize};

    #[test]
    fn tokenize_splits_camelcase_and_separators() {
        assert_eq!(tokenize("PalServer"), vec!["pal", "server"]);
        assert_eq!(tokenize("CrashReporter"), vec!["crash", "reporter"]);
        assert_eq!(
            tokenize("start_protected_game"),
            vec!["start", "protected", "game"]
        );
        assert_eq!(tokenize("EACLauncher"), vec!["eac", "launcher"]);
        assert_eq!(tokenize("Crashlands"), vec!["crashlands"]);
        assert_eq!(tokenize("Observer"), vec!["observer"]);
    }

    #[test]
    fn non_game_exe_matches_helpers_on_word_boundaries() {
        // Real helper/launcher/anti-cheat exes are still rejected.
        assert!(is_non_game_exe("PalServer.exe"));
        assert!(is_non_game_exe("EACLauncher.exe"));
        assert!(is_non_game_exe("start_protected_game.exe"));
        assert!(is_non_game_exe("EasyAntiCheat.exe"));
        assert!(is_non_game_exe("unins000.exe"));
        assert!(is_non_game_exe("SomeGame_CrashReporter.exe"));
        assert!(is_non_game_exe("vcredist_x64.exe"));
        assert!(is_non_game_exe("vconsole2.exe"));
    }

    #[test]
    fn non_game_exe_no_longer_false_positives_real_games() {
        // These previously matched via substrings ("server", "crash", "demo",
        // "test") and were wrongly excluded.
        assert!(!is_non_game_exe("Observer.exe")); // contains "server"
        assert!(!is_non_game_exe("Crashlands.exe")); // contains "crash"
        assert!(!is_non_game_exe("Protest.exe")); // contains "test"
        assert!(!is_non_game_exe("Demolition.exe")); // contains "demo"
        assert!(!is_non_game_exe("Celeste.exe"));
    }
}
