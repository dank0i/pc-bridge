//! Steam game auto-discovery
//!
//! Parses Steam's VDF files to auto-discover installed games and their executables.
//! Uses memory-mapped I/O and cached indexing for minimal overhead.

mod appinfo;
mod discovery;
mod vdf;

use std::path::PathBuf;

pub use discovery::SteamGameDiscovery;

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
    if let Ok(steam_key) = hkcu.open_subkey("Software\\Valve\\Steam") {
        if let Ok(path) = steam_key.get_value::<String, _>("SteamPath") {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }
    }

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for subkey in [
        "SOFTWARE\\WOW6432Node\\Valve\\Steam",
        "SOFTWARE\\Valve\\Steam",
    ] {
        if let Ok(steam_key) = hklm.open_subkey(subkey) {
            if let Ok(path) = steam_key.get_value::<String, _>("InstallPath") {
                let p = PathBuf::from(path);
                if p.exists() {
                    return Some(p);
                }
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
