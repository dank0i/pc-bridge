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
    /// Substrings that mark an exe as a non-game helper.
    const CONTAINS: &[&str] = &[
        "unins",
        "crash",
        "report",
        "redis",
        "launcher",
        "setup",
        "install",
        "update",
        "helper",
        "anticheat",
        "easyanticheat",
        "battleye",
        "capture",
        "message",
        "systeminfo",
        "console",
        "vconsole",
        "diagnos",
        "upload",
        "profile",
        "protected",
        "server",
        "dedicated",
    ];
    /// Prefixes that mark an exe as a redistributable/launcher.
    const STARTS_WITH: &[&str] = &[
        "vc_", "vcredist", "dotnet", "directx", "dxsetup", "physx", "uplay", "ubi", "client_",
        "start_",
    ];

    let file = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_lowercase();
    let stem = file.strip_suffix(".exe").unwrap_or(&file);

    CONTAINS.iter().any(|p| stem.contains(p)) || STARTS_WITH.iter().any(|p| stem.starts_with(p))
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
