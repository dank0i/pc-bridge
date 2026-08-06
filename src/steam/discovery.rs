//! Steam game discovery - builds process name → game ID mapping
//!
//! Performance characteristics:
//! - First run: ~100-150ms (index appinfo.vdf + parse manifests)
//! - Cached run: ~5-10ms (load cache + verify mtime)
//! - Per-game lookup: O(1) hash lookup

use log::{debug, info, warn};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use super::appinfo::AppInfoReader;
use super::vdf;

/// Cache file magic + version
const CACHE_MAGIC: u32 = 0x50435354; // "PCST"
const CACHE_VERSION: u32 = 2;

/// Safety limits for cache deserialization to prevent memory exhaustion
/// from malformed or tampered cache files.
const MAX_CACHED_GAMES: u32 = 100_000;
const MAX_STRING_LEN: u32 = 65_536;

/// Discovered game info
#[derive(Debug, Clone)]
pub struct SteamGame {
    pub app_id: u32,
    pub name: String,
    pub executable: String,    // e.g., "cs2.exe"
    pub install_path: PathBuf, // Full path to game folder
}

/// Steam game discovery result
pub struct SteamGameDiscovery {
    /// Map from executable name (lowercase, no extension) to game info
    pub games: HashMap<String, SteamGame>,
    /// Time taken to build the map
    pub build_time_ms: u64,
    /// Number of games discovered
    pub game_count: usize,
    /// Whether loaded from cache
    pub from_cache: bool,
}

impl SteamGameDiscovery {
    /// Discover all installed Steam games (async wrapper with spawn_blocking)
    ///
    /// Runs discovery on blocking threadpool to avoid blocking async runtime.
    pub async fn discover_async() -> Option<Self> {
        tokio::task::spawn_blocking(Self::discover)
            .await
            .ok()
            .flatten()
    }

    /// Force a fresh discovery, ignoring the on-disk cache (for the manual
    /// RefreshSteamGames button, so it always reflects the current install state).
    pub async fn discover_fresh_async() -> Option<Self> {
        tokio::task::spawn_blocking(Self::discover_fresh)
            .await
            .ok()
            .flatten()
    }

    /// Blocking fresh discovery: always rebuilds, then refreshes the cache.
    pub fn discover_fresh() -> Option<Self> {
        let start = Instant::now();
        let steam_path = Self::find_steam_path()?;
        let appinfo_path = steam_path.join("appcache").join("appinfo.vdf");
        let libraryfolders_path = steam_path.join("steamapps").join("libraryfolders.vdf");

        let result = Self::discover_full(&steam_path, &appinfo_path)?;
        Self::save_cache(&result, &appinfo_path, &libraryfolders_path);

        let build_time_ms = start.elapsed().as_millis() as u64;
        info!(
            "Steam discovery (forced): {} games in {}ms",
            result.game_count, build_time_ms
        );
        Some(Self {
            build_time_ms,
            from_cache: false,
            ..result
        })
    }

    /// Discover all installed Steam games (blocking)
    ///
    /// # Performance
    /// - First run: ~100-150ms (150MB appinfo.vdf indexing)
    /// - Cached run: ~5-10ms (mtime check + deserialize)
    /// - Memory: ~1KB per game + index overhead
    pub fn discover() -> Option<Self> {
        let start = Instant::now();

        // Find Steam installation
        let steam_path = Self::find_steam_path()?;
        debug!("Steam path: {:?}", steam_path);

        let appinfo_path = steam_path.join("appcache").join("appinfo.vdf");
        let libraryfolders_path = steam_path.join("steamapps").join("libraryfolders.vdf");

        // Try loading from cache first (but reject empty caches)
        if let Some(cached) = Self::load_cache(&appinfo_path, &libraryfolders_path) {
            if cached.game_count > 0 {
                let build_time_ms = start.elapsed().as_millis() as u64;
                info!(
                    "Steam discovery: {} games from cache in {}ms",
                    cached.game_count, build_time_ms
                );
                return Some(Self {
                    build_time_ms,
                    from_cache: true,
                    ..cached
                });
            } else {
                debug!("Ignoring empty cache, doing fresh discovery");
            }
        }

        // Full discovery
        let result = Self::discover_full(&steam_path, &appinfo_path)?;

        // Save to cache
        Self::save_cache(&result, &appinfo_path, &libraryfolders_path);

        let build_time_ms = start.elapsed().as_millis() as u64;
        info!(
            "Steam discovery: {} games in {}ms (cached for next run)",
            result.game_count, build_time_ms
        );

        Some(Self {
            build_time_ms,
            from_cache: false,
            ..result
        })
    }

    fn discover_full(steam_path: &Path, appinfo_path: &Path) -> Option<Self> {
        // Parse library folders - get paths AND app_ids in one pass
        let library_info = Self::get_library_info(steam_path)?;
        info!("Steam: found {} library folders", library_info.len());
        for (path, apps) in &library_info {
            info!("  Library: {} ({} apps)", path, apps.len());
        }

        // Collect all installed app_ids with their library path
        let mut installed_apps: Vec<(u32, PathBuf)> = Vec::new();
        for (lib_path, app_ids) in &library_info {
            for &app_id in app_ids {
                installed_apps.push((app_id, PathBuf::from(lib_path)));
            }
        }
        info!("Steam: {} total installed app_ids", installed_apps.len());

        if installed_apps.is_empty() {
            info!("Steam: no installed apps found, skipping discovery");
            return None;
        }

        // Open appinfo.vdf for name + executable lookup. Optional: a new/unknown
        // appinfo version (Valve bumps it periodically) must NOT disable discovery
        // entirely - the per-app appmanifest_*.acf fallback below needs nothing
        // from appinfo.
        let mut appinfo = match AppInfoReader::open(appinfo_path) {
            Ok(reader) => {
                info!("Steam: appinfo.vdf indexed {} apps", reader.app_count());
                Some(reader)
            }
            Err(e) => {
                warn!("Failed to open appinfo.vdf ({e}); using appmanifest fallback only");
                None
            }
        };

        // Log installed app_ids for debugging
        info!("Steam: looking up {} app_ids", installed_apps.len());

        // Build process name → game mapping
        let mut games = HashMap::with_capacity(installed_apps.len());
        let mut from_appinfo = 0;
        let mut from_manifest = 0;

        // Skip non-game app_ids (tools, redistributables, etc.)
        let skip_app_ids: &[u32] = &[
            228980, // Steamworks Common Redistributables
        ];

        for (app_id, library_path) in installed_apps {
            if skip_app_ids.contains(&app_id) {
                continue;
            }

            // Try appinfo.vdf first (fast), when it opened. Returns an owned
            // (name, exe) so the mutable borrow is released before we scan disk.
            let appinfo_info = appinfo.as_mut().and_then(|r| r.get_game_info(app_id));

            // Fast path: appinfo gave a real game exe (not a launcher/anti-cheat
            // wrapper). Use it directly and skip the slower folder scan.
            if let Some((name, executable)) = &appinfo_info
                && !super::is_non_game_exe(executable)
                && Self::add_game(
                    &mut games,
                    app_id,
                    name.clone(),
                    executable.clone(),
                    &library_path,
                )
                .is_some()
            {
                from_appinfo += 1;
                continue;
            }

            // Either appinfo had nothing, or it returned a wrapper that just
            // bootstraps the real game (so it is useless for process monitoring).
            // Resolve the install dir from the manifest and scan the folder for
            // the real executable.
            let manifest_path = library_path
                .join("steamapps")
                .join(format!("appmanifest_{}.acf", app_id));

            let mut disk_match: Option<String> = None; // real exe found on disk
            let mut disk_guess: Option<String> = None; // folder-name fallback guess
            let mut manifest_name: Option<String> = None;
            if let Ok(content) = fs::read_to_string(&manifest_path)
                && let Some((_, mname, installdir)) = vdf::extract_appmanifest_fields(&content)
                // installdir must be a single folder name: reject path separators
                // and traversal so a crafted .acf can't redirect the exe scan
                // outside steamapps/common.
                && !installdir.is_empty()
                && !installdir.contains(['/', '\\'])
                && !installdir.contains("..")
            {
                manifest_name = Some(mname);
                let game_path = library_path
                    .join("steamapps")
                    .join("common")
                    .join(&installdir);
                match Self::find_game_executable(&game_path) {
                    Some((exe, true)) => disk_match = Some(exe),
                    Some((exe, false)) => disk_guess = Some(exe),
                    None => {}
                }
            }

            // Resolve the exe by priority:
            //   1. a real exe found on disk (upgrades a genuine wrapper);
            //   2. the appinfo-declared exe, even if it tripped the blacklist - it
            //      is Steam's own launch target, so it beats a folder-name guess
            //      and rescues a real exe that merely contains a blacklist word
            //      (e.g. a dedicated-server "PalServer.exe");
            //   3. the folder-name guess, only when appinfo gave us nothing.
            let name = appinfo_info
                .as_ref()
                .map(|(n, _)| n.clone())
                .or(manifest_name);
            let appinfo_exe = appinfo_info.map(|(_, e)| e);

            let (exe, from_appinfo_source) = if let Some(exe) = disk_match {
                (Some(exe), false)
            } else if let Some(exe) = appinfo_exe {
                (Some(exe), true)
            } else {
                (disk_guess, false)
            };

            if let (Some(exe), Some(name)) = (exe, name)
                && Self::add_game(&mut games, app_id, name, exe, &library_path).is_some()
            {
                if from_appinfo_source {
                    from_appinfo += 1;
                } else {
                    from_manifest += 1;
                }
            }
        }

        info!(
            "Steam: {} from appinfo, {} from manifests",
            from_appinfo, from_manifest
        );
        let game_count = games.len();
        info!("Steam: {} unique games total", game_count);

        Some(Self {
            games,
            build_time_ms: 0,
            game_count,
            from_cache: false,
        })
    }

    /// Add a game to the map, returns the key if successful
    fn add_game(
        games: &mut HashMap<String, SteamGame>,
        app_id: u32,
        name: String,
        executable: String,
        library_path: &Path,
    ) -> Option<String> {
        let exe_name = Path::new(&executable)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&executable);

        // Key: lowercase exe name without extension
        let key = exe_name.to_lowercase();
        // Lowercase FIRST: strip_suffix is case-sensitive, so stripping before
        // lowercasing leaves ".EXE" attached and the key never matches.
        let key = key.strip_suffix(".exe").unwrap_or(&key).to_string();

        // Skip if empty key
        if key.is_empty() {
            return None;
        }

        let install_path = library_path.join("steamapps").join("common");

        games.insert(
            key.clone(),
            SteamGame {
                app_id,
                name,
                executable: exe_name.to_string(),
                install_path,
            },
        );

        Some(key)
    }

    /// Find the main executable in a game folder.
    ///
    /// Strategy: find exe that matches folder name, or largest exe that looks
    /// like a game. Returns `(exe, confident)`: `confident` is true when a real
    /// on-disk exe was matched, false when we fell back to a `<folder>.exe`
    /// guess (no real match). Callers prefer a confident match over an
    /// appinfo-declared exe, and an appinfo exe over a non-confident guess.
    fn find_game_executable(game_path: &Path) -> Option<(String, bool)> {
        if !game_path.is_dir() {
            return None;
        }

        let folder_name = game_path.file_name()?.to_str()?.to_lowercase();
        let mut candidates: Vec<(String, u64, bool)> = Vec::new(); // (name, size, matches_folder)

        // Search root and common subdirectories
        Self::scan_for_executables(game_path, &folder_name, &mut candidates);

        // Common game exe locations
        let subdirs = [
            "bin",
            "Binaries",
            "Binaries/Win64",
            "Binaries/Win64/Shipping",
            "game/bin/win64",
            "x64",
            "game",
            "Win64",
            "Win64/Shipping",
            "Engine/Binaries/Win64", // Unreal games
        ];

        for subdir in subdirs {
            let sub_path = game_path.join(subdir);
            if sub_path.is_dir() {
                Self::scan_for_executables(&sub_path, &folder_name, &mut candidates);
            }
        }

        // Also scan immediate subdirectories (one level deep)
        if let Ok(entries) = fs::read_dir(game_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    Self::scan_for_executables(&path, &folder_name, &mut candidates);
                }
            }
        }

        // Sort: prefer exact/short matches, avoid trial/demo, then by size
        candidates.sort_by(|a, b| {
            // Penalize trial/demo/test versions, matched on word boundaries so
            // "Protest" isn't a "test" and "Demolition" isn't a "demo".
            const TRIAL_MARKERS: &[&str] = &["trial", "demo", "test", "benchmark"];
            let a_trial = super::has_marker_token(&a.0, TRIAL_MARKERS);
            let b_trial = super::has_marker_token(&b.0, TRIAL_MARKERS);
            if a_trial != b_trial {
                return if a_trial {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }

            // Prefer folder name matches
            match (a.2, b.2) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }

            // Among matches, prefer shorter names (closer to exact match)
            if a.2 && b.2 {
                let len_cmp = a.0.len().cmp(&b.0.len());
                if len_cmp != std::cmp::Ordering::Equal {
                    return len_cmp;
                }
            }

            // Finally, prefer larger files
            b.1.cmp(&a.1)
        });

        // Return best candidate if one was found (confident on-disk match).
        if let Some((name, _, _)) = candidates.first() {
            return Some((name.clone(), true));
        }

        // Fallback: use folder name as exe name (e.g., "3DMark" -> "3DMark.exe").
        // This works even if the exe is deeply nested or not found, but it is a
        // guess - flagged non-confident so callers can prefer a real appinfo exe.
        let folder_name = game_path.file_name()?.to_str()?;
        Some((format!("{}.exe", folder_name), false))
    }

    /// Scan a directory for game executables
    fn scan_for_executables(
        dir: &Path,
        folder_name: &str,
        candidates: &mut Vec<(String, u64, bool)>,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        // Pre-compute cleaned folder name once (was recomputed per file)
        let folder_clean = folder_name.replace(['-', '_', ' ', '.'], "").to_lowercase();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Some(ext) = path.extension() else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("exe") {
                continue;
            }

            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_lowercase();
            let stem = lower.strip_suffix(".exe").unwrap_or(&lower);

            // Skip launchers, anti-cheat wrappers, installers, and other helpers.
            // Pass the ORIGINAL-case name so is_non_game_exe can see camelCase
            // token boundaries (PalServer -> pal|server).
            if super::is_non_game_exe(name) {
                continue;
            }

            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

            // Check if exe name matches folder name (fuzzy)
            // bf6 matches "Battlefield 6", cs2 matches "Counter-Strike 2", etc.
            let exe_clean = stem.replace(['-', '_', ' ', '.'], "").to_lowercase();

            // Match if: folder contains exe OR exe contains folder OR they share significant overlap
            // Also match common abbreviation patterns (bf6 = battlefield6, cs2 = counterstrike2)
            let matches = folder_clean.contains(&exe_clean)
                || exe_clean.contains(&folder_clean)
                || folder_clean.starts_with(&exe_clean)
                || exe_clean.starts_with(&folder_clean)
                || Self::abbreviation_matches(&exe_clean, &folder_clean);

            candidates.push((name.to_string(), size, matches));
        }
    }

    /// Check if exe name is an abbreviation of the folder name
    /// e.g., "bf6" matches "battlefield6", "cs2" matches "counterstrike2"
    fn abbreviation_matches(exe: &str, folder: &str) -> bool {
        // Skip if exe is too long to be an abbreviation
        if exe.len() > 8 || exe.len() >= folder.len() {
            return false;
        }

        // Check if exe letters appear in order in folder
        // bf6 -> b...f...6 in "battlefield6"
        let mut folder_chars = folder.chars().peekable();
        for exe_char in exe.chars() {
            // Find this char in remaining folder
            loop {
                match folder_chars.next() {
                    Some(fc) if fc == exe_char => break, // Found match
                    Some(_) => continue,                 // Keep looking
                    None => return false,                // Not found
                }
            }
        }
        true
    }

    // =========================================================================
    // Caching
    // =========================================================================

    fn cache_path() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var("LOCALAPPDATA")
                .ok()
                .map(|p| PathBuf::from(p).join("pc-bridge").join("steam_cache.bin"))
        }
        #[cfg(unix)]
        {
            std::env::var("HOME").ok().map(|p| {
                PathBuf::from(p)
                    .join(".cache")
                    .join("pc-bridge")
                    .join("steam_cache.bin")
            })
        }
    }

    fn get_file_mtime(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    }

    fn load_cache(appinfo_path: &Path, libraryfolders_path: &Path) -> Option<Self> {
        let cache_path = Self::cache_path()?;
        let file = File::open(&cache_path).ok()?;
        let mut reader = BufReader::new(file);

        // Read header
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).ok()?;
        if u32::from_le_bytes(buf) != CACHE_MAGIC {
            return None;
        }

        reader.read_exact(&mut buf).ok()?;
        if u32::from_le_bytes(buf) != CACHE_VERSION {
            return None;
        }

        // Read stored mtimes: appinfo.vdf (names/exes) + libraryfolders.vdf (installed set)
        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8).ok()?;
        let cached_appinfo_mtime = u64::from_le_bytes(buf8);
        reader.read_exact(&mut buf8).ok()?;
        let cached_lib_mtime = u64::from_le_bytes(buf8);

        // appinfo.vdf changes when Valve refreshes metadata; libraryfolders.vdf
        // changes on install/uninstall. Invalidate if EITHER moved, otherwise an
        // uninstall leaves the game list stale until steam_cache.bin is deleted.
        if Self::get_file_mtime(appinfo_path)? != cached_appinfo_mtime {
            debug!("Cache invalidated: appinfo.vdf modified");
            return None;
        }
        if Self::get_file_mtime(libraryfolders_path)? != cached_lib_mtime {
            debug!("Cache invalidated: libraryfolders.vdf modified (install/uninstall)");
            return None;
        }

        // Read game count
        reader.read_exact(&mut buf).ok()?;
        let game_count = u32::from_le_bytes(buf);
        if game_count > MAX_CACHED_GAMES {
            warn!("Cache game count too large ({game_count}), ignoring cache");
            return None;
        }
        let game_count = game_count as usize;

        // Read games
        let mut games = HashMap::with_capacity(game_count);
        for _ in 0..game_count {
            let game = Self::read_game(&mut reader)?;
            let key = game.executable.to_lowercase();
            let key = key.strip_suffix(".exe").unwrap_or(&key).to_string();
            games.insert(key, game);
        }

        Some(Self {
            games,
            build_time_ms: 0,
            game_count,
            from_cache: true,
        })
    }

    fn read_game(reader: &mut BufReader<File>) -> Option<SteamGame> {
        let mut buf4 = [0u8; 4];

        // app_id
        reader.read_exact(&mut buf4).ok()?;
        let app_id = u32::from_le_bytes(buf4);

        // name (length-prefixed)
        reader.read_exact(&mut buf4).ok()?;
        let len = u32::from_le_bytes(buf4);
        if len > MAX_STRING_LEN {
            return None;
        }
        let mut name_buf = vec![0u8; len as usize];
        reader.read_exact(&mut name_buf).ok()?;
        let name = String::from_utf8(name_buf).ok()?;

        // executable (length-prefixed)
        reader.read_exact(&mut buf4).ok()?;
        let len = u32::from_le_bytes(buf4);
        if len > MAX_STRING_LEN {
            return None;
        }
        let mut exe_buf = vec![0u8; len as usize];
        reader.read_exact(&mut exe_buf).ok()?;
        let executable = String::from_utf8(exe_buf).ok()?;

        // install_path (length-prefixed)
        reader.read_exact(&mut buf4).ok()?;
        let len = u32::from_le_bytes(buf4);
        if len > MAX_STRING_LEN {
            return None;
        }
        let mut path_buf = vec![0u8; len as usize];
        reader.read_exact(&mut path_buf).ok()?;
        let install_path = PathBuf::from(String::from_utf8(path_buf).ok()?);

        Some(SteamGame {
            app_id,
            name,
            executable,
            install_path,
        })
    }

    fn save_cache(&self, appinfo_path: &Path, libraryfolders_path: &Path) {
        let Some(cache_path) = Self::cache_path() else {
            return;
        };

        // Create cache directory
        if let Some(parent) = cache_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        // Write to a temporary file first, then atomically rename into place.
        // This prevents corruption if the process crashes mid-write.
        let tmp_path = cache_path.with_extension("tmp");

        let Ok(file) = File::create(&tmp_path) else {
            return;
        };
        let mut writer = BufWriter::new(file);

        // Write header
        let _ = writer.write_all(&CACHE_MAGIC.to_le_bytes());
        let _ = writer.write_all(&CACHE_VERSION.to_le_bytes());

        // Write appinfo.vdf mtime + libraryfolders.vdf mtime (installed-set signal)
        let appinfo_mtime = Self::get_file_mtime(appinfo_path).unwrap_or(0);
        let _ = writer.write_all(&appinfo_mtime.to_le_bytes());
        let lib_mtime = Self::get_file_mtime(libraryfolders_path).unwrap_or(0);
        let _ = writer.write_all(&lib_mtime.to_le_bytes());

        // Write game count
        let _ = writer.write_all(&(self.game_count as u32).to_le_bytes());

        // Write games
        for game in self.games.values() {
            Self::write_game(&mut writer, game);
        }

        if writer.flush().is_err() {
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        // Drop the writer/file handle before renaming
        drop(writer);

        if fs::rename(&tmp_path, &cache_path).is_err() {
            let _ = fs::remove_file(&tmp_path);
            return;
        }

        debug!("Saved Steam cache to {:?}", cache_path);
    }

    fn write_game(writer: &mut BufWriter<File>, game: &SteamGame) {
        // app_id
        let _ = writer.write_all(&game.app_id.to_le_bytes());

        // name
        let name_bytes = game.name.as_bytes();
        let _ = writer.write_all(&(name_bytes.len() as u32).to_le_bytes());
        let _ = writer.write_all(name_bytes);

        // executable
        let exe_bytes = game.executable.as_bytes();
        let _ = writer.write_all(&(exe_bytes.len() as u32).to_le_bytes());
        let _ = writer.write_all(exe_bytes);

        // install_path
        let path_str = game.install_path.to_string_lossy();
        let path_bytes = path_str.as_bytes();
        let _ = writer.write_all(&(path_bytes.len() as u32).to_le_bytes());
        let _ = writer.write_all(path_bytes);
    }

    // =========================================================================
    // Steam path discovery
    // =========================================================================

    /// Find Steam installation path (delegates to shared implementation)
    fn find_steam_path() -> Option<PathBuf> {
        super::find_steam_path()
    }

    /// Get library info from libraryfolders.vdf (paths + app_ids)
    fn get_library_info(steam_path: &Path) -> Option<Vec<(String, Vec<u32>)>> {
        let vdf_path = steam_path.join("steamapps").join("libraryfolders.vdf");
        debug!("Reading libraryfolders.vdf from {:?}", vdf_path);

        let content = match fs::read_to_string(&vdf_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read libraryfolders.vdf: {}", e);
                return None;
            }
        };

        let mut info = vdf::extract_library_info(&content);
        debug!("VDF parser returned {} libraries:", info.len());
        for (path, apps) in &info {
            debug!("  {} - {} apps", path, apps.len());
        }

        // Also include main Steam path if not in list
        let steam_path_str = steam_path.to_string_lossy().to_string();
        if !info.iter().any(|(p, _)| p == &steam_path_str) {
            // If main path not included, add it with empty apps (will scan later)
            debug!("Adding main Steam path: {}", steam_path_str);
            info.insert(0, (steam_path_str, vec![]));
        }

        Some(info)
    }

    /// Lookup game by process name
    #[inline]
    pub fn lookup(&self, process_name: &str) -> Option<&SteamGame> {
        // Normalize: lowercase, remove .exe
        // Lowercase FIRST: strip_suffix is case-sensitive, so ".EXE" would
        // survive and never match the stored key.
        let key = process_name.to_lowercase();
        let key = key.strip_suffix(".exe").unwrap_or(&key);

        self.games.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- abbreviation_matches tests --

    #[test]
    fn test_abbreviation_bf6_battlefield6() {
        assert!(SteamGameDiscovery::abbreviation_matches(
            "bf6",
            "battlefield6"
        ));
    }

    #[test]
    fn test_abbreviation_cs2_counterstrike2() {
        assert!(SteamGameDiscovery::abbreviation_matches(
            "cs2",
            "counterstrike2"
        ));
    }

    #[test]
    fn test_abbreviation_too_long() {
        // exe longer than 8 chars is not an abbreviation
        assert!(!SteamGameDiscovery::abbreviation_matches(
            "longexename",
            "longexenameandmore"
        ));
    }

    #[test]
    fn test_abbreviation_same_length() {
        // exe must be shorter than folder
        assert!(!SteamGameDiscovery::abbreviation_matches("game", "game"));
    }

    #[test]
    fn test_abbreviation_no_match() {
        assert!(!SteamGameDiscovery::abbreviation_matches(
            "xyz",
            "battlefield"
        ));
    }

    #[test]
    fn test_abbreviation_single_char() {
        assert!(SteamGameDiscovery::abbreviation_matches("b", "battlefield"));
    }

    #[test]
    fn test_abbreviation_out_of_order() {
        // "fb" can't match "battlefield" because f appears before b in "fb" but
        // in "battlefield" b comes before f - wait, actually b-a-t-t-l-e-f...
        // 'f' first: find 'f' in "battlefield" → not found before 'b' wait...
        // The algorithm finds chars in order. "fb": find 'f' in battlefield → pos 6,
        // then find 'b' in remainder "ield" → not found. So false.
        assert!(!SteamGameDiscovery::abbreviation_matches(
            "fb",
            "battlefield"
        ));
    }

    // -- add_game tests --

    #[test]
    fn test_add_game_basic() {
        let mut games = HashMap::new();
        let lib = PathBuf::from("C:\\Steam");
        let key = SteamGameDiscovery::add_game(
            &mut games,
            730,
            "Counter-Strike 2".to_string(),
            "cs2.exe".to_string(),
            &lib,
        );
        assert_eq!(key, Some("cs2".to_string()));
        assert_eq!(games.len(), 1);
        let game = games.get("cs2").unwrap();
        assert_eq!(game.app_id, 730);
        assert_eq!(game.name, "Counter-Strike 2");
        assert_eq!(game.executable, "cs2.exe");
    }

    #[test]
    fn test_add_game_strips_exe_and_lowercases() {
        let mut games = HashMap::new();
        let lib = PathBuf::from("C:\\Steam");
        let key = SteamGameDiscovery::add_game(
            &mut games,
            12345,
            "My Game".to_string(),
            "MyGame.exe".to_string(),
            &lib,
        );
        assert_eq!(key, Some("mygame".to_string()));
    }

    #[test]
    fn test_add_game_no_extension() {
        let mut games = HashMap::new();
        let lib = PathBuf::from("/steam");
        let key = SteamGameDiscovery::add_game(
            &mut games,
            999,
            "Linux Game".to_string(),
            "linuxgame".to_string(),
            &lib,
        );
        assert_eq!(key, Some("linuxgame".to_string()));
    }

    #[test]
    fn test_add_game_empty_exe() {
        let mut games = HashMap::new();
        let lib = PathBuf::from("C:\\Steam");
        let key = SteamGameDiscovery::add_game(
            &mut games,
            0,
            "Empty".to_string(),
            ".exe".to_string(),
            &lib,
        );
        // key would be "" after stripping .exe, should return None
        assert_eq!(key, None);
        assert!(games.is_empty());
    }

    #[test]
    fn test_add_game_install_path() {
        let mut games = HashMap::new();
        let lib = PathBuf::from("D:\\SteamLibrary");
        SteamGameDiscovery::add_game(
            &mut games,
            440,
            "Team Fortress 2".to_string(),
            "hl2.exe".to_string(),
            &lib,
        );
        let game = games.get("hl2").unwrap();
        let expected = PathBuf::from("D:\\SteamLibrary")
            .join("steamapps")
            .join("common");
        assert_eq!(game.install_path, expected);
    }

    // -- lookup tests --

    #[test]
    fn test_lookup_with_exe_extension() {
        let mut games = HashMap::new();
        games.insert(
            "cs2".to_string(),
            SteamGame {
                app_id: 730,
                name: "Counter-Strike 2".to_string(),
                executable: "cs2.exe".to_string(),
                install_path: PathBuf::from("C:\\Steam\\steamapps\\common"),
            },
        );
        let discovery = SteamGameDiscovery {
            games,
            build_time_ms: 0,
            game_count: 1,
            from_cache: false,
        };
        assert!(discovery.lookup("cs2.exe").is_some());
        assert_eq!(discovery.lookup("cs2.exe").unwrap().app_id, 730);
    }

    #[test]
    fn test_lookup_without_extension() {
        let mut games = HashMap::new();
        games.insert(
            "cs2".to_string(),
            SteamGame {
                app_id: 730,
                name: "CS2".to_string(),
                executable: "cs2.exe".to_string(),
                install_path: PathBuf::from("/steam"),
            },
        );
        let discovery = SteamGameDiscovery {
            games,
            build_time_ms: 0,
            game_count: 1,
            from_cache: false,
        };
        assert!(discovery.lookup("cs2").is_some());
    }

    #[test]
    fn test_lookup_case_insensitive() {
        let mut games = HashMap::new();
        games.insert(
            "cs2".to_string(),
            SteamGame {
                app_id: 730,
                name: "CS2".to_string(),
                executable: "cs2.exe".to_string(),
                install_path: PathBuf::from("/steam"),
            },
        );
        let discovery = SteamGameDiscovery {
            games,
            build_time_ms: 0,
            game_count: 1,
            from_cache: false,
        };
        assert!(discovery.lookup("CS2.exe").is_some());
    }

    #[test]
    fn test_lookup_not_found() {
        let discovery = SteamGameDiscovery {
            games: HashMap::new(),
            build_time_ms: 0,
            game_count: 0,
            from_cache: false,
        };
        assert!(discovery.lookup("nonexistent.exe").is_none());
    }

    // -- find_game_executable confidence flag --

    #[test]
    fn test_find_game_executable_confident_on_real_match() {
        // A real exe that fuzzy-matches the folder name is a confident match.
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("Celeste");
        fs::create_dir(&game_dir).unwrap();
        fs::write(game_dir.join("Celeste.exe"), b"x").unwrap();

        let (exe, confident) =
            SteamGameDiscovery::find_game_executable(&game_dir).expect("should resolve");
        assert_eq!(exe, "Celeste.exe");
        assert!(confident, "a real on-disk match must be confident");
    }

    #[test]
    fn test_find_game_executable_guess_when_only_blacklisted_exe() {
        // Folder holds only a blacklisted exe (dedicated-server style name): the
        // scanner rejects it, so we fall back to the folder-name guess flagged
        // NON-confident, letting discovery prefer the real appinfo exe instead.
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("Palworld");
        fs::create_dir(&game_dir).unwrap();
        fs::write(game_dir.join("PalServer.exe"), b"x").unwrap();

        let (exe, confident) =
            SteamGameDiscovery::find_game_executable(&game_dir).expect("should resolve");
        assert_eq!(exe, "Palworld.exe", "falls back to folder-name guess");
        assert!(!confident, "a folder-name guess must not be confident");
    }

    #[test]
    fn test_find_game_executable_none_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");
        assert!(SteamGameDiscovery::find_game_executable(&missing).is_none());
    }
}
