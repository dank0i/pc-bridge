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

/// Unix seconds at which the running Steam client started, or None when Steam
/// is not running or the answer cannot be determined.
///
/// This is the boundary of Steam's own "Completed" list: the client forgets
/// every finished download when it restarts, so a completion timestamp older
/// than this belongs to a previous session and is not shown. Measured against
/// the live client: completions at 22:35 survived until Steam was restarted at
/// 23:11, after which only later ones were listed.
///
/// Returning None is "cannot determine", never "Steam just started"; a caller
/// must not substitute now().
#[cfg(windows)]
pub fn steam_started_at() -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // FILETIME counts 100ns intervals from 1601-01-01; Unix counts seconds from
    // 1970-01-01. This is the gap, in seconds.
    const FILETIME_TO_UNIX_SECS: u64 = 11_644_473_600;
    const HUNDRED_NS_PER_SEC: u64 = 10_000_000;

    let mut pid = None;
    // SAFETY: standard Win32 process enumeration, mirroring
    // process_watcher::snapshot_all_processes. Every handle is closed.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return None;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &raw mut entry).is_ok() {
            loop {
                let nul = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..nul])
                    .eq_ignore_ascii_case("steam.exe")
                {
                    pid = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snapshot, &raw mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    let pid = pid?;

    // SAFETY: the handle from OpenProcess is closed on every path below.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = FILETIME::default();
        let (mut exit, mut kernel, mut user) = Default::default();
        let ok = GetProcessTimes(
            handle,
            &raw mut created,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok {
            return None;
        }
        let ticks = ((created.dwHighDateTime as u64) << 32) | u64::from(created.dwLowDateTime);
        (ticks / HUNDRED_NS_PER_SEC).checked_sub(FILETIME_TO_UNIX_SECS)
    }
}

/// Unix seconds at which the running Steam client started. See the Windows twin
/// for what this is for.
///
/// Reads `/proc`, so it answers on Linux and returns None on macOS, which has
/// no `/proc`. That is the honest answer there rather than a fabricated one.
#[cfg(unix)]
pub fn steam_started_at() -> Option<u64> {
    // Field 22 of /proc/<pid>/stat is the process start time in clock ticks
    // since boot. USER_HZ is 100 on every Linux ABI in practice and is fixed
    // for the kernel ABI regardless of the configured tick rate.
    const USER_HZ: u64 = 100;

    let boot = std::fs::read_to_string("/proc/stat").ok()?;
    let btime: u64 = boot
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;

    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let path = entry.path();
        let Some(pid) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !pid.bytes().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if comm.trim() != "steam" {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(path.join("stat")) else {
            continue;
        };
        if let Some(start) = parse_proc_stat_starttime(&stat) {
            return Some(btime + start / USER_HZ);
        }
    }
    None
}

/// Field 22 (1-indexed) of /proc/<pid>/stat, the start time in clock ticks.
///
/// Split out so it is unit-testable off Linux. The executable name in field 2
/// is wrapped in parentheses and may itself contain spaces or parentheses, so
/// fields are counted from after the LAST `)`, never by splitting the whole
/// line on whitespace.
#[cfg(unix)]
fn parse_proc_stat_starttime(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    // After the closing paren, field 3 (state) is first, so start time is the
    // 20th field from here.
    after.split_whitespace().nth(19)?.parse().ok()
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
    // -- parse_proc_stat_starttime --
    // The executable name is parenthesised and may contain spaces and
    // parentheses, which is why fields are counted from the LAST ')'.

    #[test]
    #[cfg(unix)]
    fn test_proc_stat_starttime_plain_name() {
        let stat = "1234 (steam) S 1 1234 1234 0 -1 4194560 100 0 0 0 5 2 0 0 20 0 30 0 987654                     123456 789 18446744073709551615";
        assert_eq!(super::parse_proc_stat_starttime(stat), Some(987_654));
    }

    #[test]
    #[cfg(unix)]
    fn test_proc_stat_starttime_name_with_spaces_and_parens() {
        // A name like "steam (beta) x" would break a naive whitespace split.
        let stat = "1234 (steam (beta) x) S 1 1234 1234 0 -1 4194560 100 0 0 0 5 2 0 0 20 0 30 0                     555 123456 789";
        assert_eq!(super::parse_proc_stat_starttime(stat), Some(555));
    }

    #[test]
    #[cfg(unix)]
    fn test_proc_stat_starttime_truncated_is_none() {
        assert_eq!(
            super::parse_proc_stat_starttime("1234 (steam) S 1 2 3"),
            None
        );
        assert_eq!(super::parse_proc_stat_starttime("no parens here"), None);
    }

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
