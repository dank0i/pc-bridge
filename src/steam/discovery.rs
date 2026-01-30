//! Steam game discovery - builds process name → game ID mapping
//!
//! Performance characteristics:
//! - First run: ~100-150ms (index appinfo.vdf + parse manifests)
//! - Cached run: ~5-10ms (load cache + verify mtime)
//! - Per-game lookup: O(1) hash lookup

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, debug, warn};

use super::vdf;
use super::appinfo::AppInfoReader;

/// Cache file magic + version
const CACHE_MAGIC: u32 = 0x50435354; // "PCST"
const CACHE_VERSION: u32 = 1;

/// Discovered game info
#[derive(Debug, Clone)]
pub struct SteamGame {
    pub app_id: u32,
    pub name: String,
    pub executable: String,      // e.g., "cs2.exe"
    pub install_path: PathBuf,   // Full path to game folder
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
        
        // Try loading from cache first
        if let Some(cached) = Self::load_cache(&appinfo_path) {
            let build_time_ms = start.elapsed().as_millis() as u64;
            info!("Steam discovery: {} games from cache in {}ms", cached.game_count, build_time_ms);
            return Some(Self {
                build_time_ms,
                from_cache: true,
                ..cached
            });
        }
        
        // Full discovery
        let result = Self::discover_full(&steam_path, &appinfo_path)?;
        
        // Save to cache
        Self::save_cache(&result, &appinfo_path);
        
        let build_time_ms = start.elapsed().as_millis() as u64;
        info!("Steam discovery: {} games in {}ms (cached for next run)", result.game_count, build_time_ms);
        
        Some(Self {
            build_time_ms,
            from_cache: false,
            ..result
        })
    }
    
    fn discover_full(steam_path: &Path, appinfo_path: &Path) -> Option<Self> {
        // Parse library folders
        let library_paths = Self::get_library_paths(steam_path)?;
        debug!("Found {} library paths", library_paths.len());
        
        // Collect installed app IDs and info from manifests
        let installed_games = Self::get_installed_games(&library_paths);
        debug!("Found {} installed games", installed_games.len());
        
        if installed_games.is_empty() {
            return None;
        }
        
        // Open appinfo.vdf for executable lookup
        let mut appinfo = match AppInfoReader::open(appinfo_path) {
            Ok(reader) => {
                debug!("Indexed {} apps from appinfo.vdf", reader.app_count());
                Some(reader)
            }
            Err(e) => {
                warn!("Failed to open appinfo.vdf: {}", e);
                None
            }
        };
        
        // Build process name → game mapping
        let mut games = HashMap::with_capacity(installed_games.len());
        
        for (app_id, name, install_dir, library_path) in installed_games {
            // Get executable from appinfo.vdf
            let executable = appinfo.as_mut()
                .and_then(|reader| reader.get_executable(app_id))
                .unwrap_or_default();
            
            if executable.is_empty() {
                continue;
            }
            
            // Extract just the exe filename
            let exe_name = Path::new(&executable)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&executable);
            
            // Key: lowercase exe name without extension
            let key = exe_name
                .strip_suffix(".exe")
                .unwrap_or(exe_name)
                .to_lowercase();
            
            let install_path = library_path
                .join("steamapps")
                .join("common")
                .join(&install_dir);
            
            games.insert(key, SteamGame {
                app_id,
                name,
                executable: exe_name.to_string(),
                install_path,
            });
        }
        
        let game_count = games.len();
        
        Some(Self {
            games,
            build_time_ms: 0,
            game_count,
            from_cache: false,
        })
    }
    
    // =========================================================================
    // Caching
    // =========================================================================
    
    fn cache_path() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var("LOCALAPPDATA").ok()
                .map(|p| PathBuf::from(p).join("pc-bridge").join("steam_cache.bin"))
        }
        #[cfg(unix)]
        {
            std::env::var("HOME").ok()
                .map(|p| PathBuf::from(p).join(".cache").join("pc-bridge").join("steam_cache.bin"))
        }
    }
    
    fn get_file_mtime(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    }
    
    fn load_cache(appinfo_path: &Path) -> Option<Self> {
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
        
        // Read stored mtime
        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8).ok()?;
        let cached_mtime = u64::from_le_bytes(buf8);
        
        // Check if appinfo.vdf has changed
        let current_mtime = Self::get_file_mtime(appinfo_path)?;
        if current_mtime != cached_mtime {
            debug!("Cache invalidated: appinfo.vdf modified");
            return None;
        }
        
        // Read game count
        reader.read_exact(&mut buf).ok()?;
        let game_count = u32::from_le_bytes(buf) as usize;
        
        // Read games
        let mut games = HashMap::with_capacity(game_count);
        for _ in 0..game_count {
            let game = Self::read_game(&mut reader)?;
            let key = game.executable
                .strip_suffix(".exe")
                .unwrap_or(&game.executable)
                .to_lowercase();
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
        let len = u32::from_le_bytes(buf4) as usize;
        let mut name_buf = vec![0u8; len];
        reader.read_exact(&mut name_buf).ok()?;
        let name = String::from_utf8(name_buf).ok()?;
        
        // executable (length-prefixed)
        reader.read_exact(&mut buf4).ok()?;
        let len = u32::from_le_bytes(buf4) as usize;
        let mut exe_buf = vec![0u8; len];
        reader.read_exact(&mut exe_buf).ok()?;
        let executable = String::from_utf8(exe_buf).ok()?;
        
        // install_path (length-prefixed)
        reader.read_exact(&mut buf4).ok()?;
        let len = u32::from_le_bytes(buf4) as usize;
        let mut path_buf = vec![0u8; len];
        reader.read_exact(&mut path_buf).ok()?;
        let install_path = PathBuf::from(String::from_utf8(path_buf).ok()?);
        
        Some(SteamGame {
            app_id,
            name,
            executable,
            install_path,
        })
    }
    
    fn save_cache(&self, appinfo_path: &Path) {
        let Some(cache_path) = Self::cache_path() else { return };
        
        // Create cache directory
        if let Some(parent) = cache_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        
        let Ok(file) = File::create(&cache_path) else { return };
        let mut writer = BufWriter::new(file);
        
        // Write header
        let _ = writer.write_all(&CACHE_MAGIC.to_le_bytes());
        let _ = writer.write_all(&CACHE_VERSION.to_le_bytes());
        
        // Write appinfo.vdf mtime
        let mtime = Self::get_file_mtime(appinfo_path).unwrap_or(0);
        let _ = writer.write_all(&mtime.to_le_bytes());
        
        // Write game count
        let _ = writer.write_all(&(self.game_count as u32).to_le_bytes());
        
        // Write games
        for game in self.games.values() {
            Self::write_game(&mut writer, game);
        }
        
        let _ = writer.flush();
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
    
    /// Find Steam installation path
    #[cfg(windows)]
    fn find_steam_path() -> Option<PathBuf> {
        use winreg::enums::*;
        use winreg::RegKey;
        
        // Try registry first (most reliable)
        if let Ok(hklm) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey("SOFTWARE\\WOW6432Node\\Valve\\Steam")
        {
            if let Ok(path) = hklm.get_value::<String, _>("InstallPath") {
                return Some(PathBuf::from(path));
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
            if p.join("steam.exe").exists() {
                return Some(p);
            }
        }
        
        None
    }
    
    #[cfg(unix)]
    fn find_steam_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        
        let paths = [
            format!("{}/.steam/steam", home),
            format!("{}/.local/share/Steam", home),
            format!("{}/.steam/debian-installation", home),
        ];
        
        for path in paths {
            let p = PathBuf::from(&path);
            if p.join("steamapps").exists() {
                return Some(p);
            }
        }
        
        None
    }
    
    /// Get all Steam library paths from libraryfolders.vdf
    fn get_library_paths(steam_path: &Path) -> Option<Vec<PathBuf>> {
        let vdf_path = steam_path.join("steamapps").join("libraryfolders.vdf");
        let content = fs::read_to_string(&vdf_path).ok()?;
        
        let paths = vdf::extract_library_paths(&content);
        
        // Also include main Steam path if not in list
        let mut result: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        if !result.iter().any(|p| p == steam_path) {
            result.insert(0, steam_path.to_path_buf());
        }
        
        Some(result)
    }
    
    /// Get installed games from appmanifest files
    /// Returns: Vec<(app_id, name, install_dir, library_path)>
    fn get_installed_games(library_paths: &[PathBuf]) -> Vec<(u32, String, String, PathBuf)> {
        let mut games = Vec::new();
        
        for library_path in library_paths {
            let steamapps = library_path.join("steamapps");
            
            // Read directory and filter appmanifest files
            if let Ok(entries) = fs::read_dir(&steamapps) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("appmanifest_") && name.ends_with(".acf") {
                            if let Ok(content) = fs::read_to_string(&path) {
                                if let Some((app_id, name, install_dir)) = vdf::extract_appmanifest_fields(&content) {
                                    games.push((app_id, name, install_dir, library_path.clone()));
                                }
                            }
                        }
                    }
                }
            }
        }
        
        games
    }
    
    /// Lookup game by process name
    #[inline]
    pub fn lookup(&self, process_name: &str) -> Option<&SteamGame> {
        // Normalize: lowercase, remove .exe
        let key = process_name
            .strip_suffix(".exe")
            .unwrap_or(process_name)
            .to_lowercase();
        
        self.games.get(&key)
    }
    
    /// Get all discovered games
    pub fn all_games(&self) -> impl Iterator<Item = &SteamGame> {
        self.games.values()
    }
}
