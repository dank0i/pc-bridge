//! Steam update detection sensor - monitors ACF files for games being updated
//!
//! Uses filesystem watcher (`notify` crate) for instant detection of ACF manifest changes.
//! Falls back to periodic polling if the watcher fails.

use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;

/// Pre-built JSON attributes for the idle (no updates) state.
/// Avoids re-creating the same `serde_json::Value` every publish cycle.
static IDLE_STEAM_ATTRS: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::json!({"updating_games": [], "count": 0}));
use tokio::time::Duration;

use crate::AppState;
use crate::sensors::steam_progress::{self, DownloadEstimator, LogEvent};

/// How often to read new content_log lines and advance the estimate.
///
/// 2s: Steam reports its transfer rate several times a minute, and between those
/// readings the estimate is pure integration, so a shorter tick simply makes the
/// bar move more smoothly at no real cost (one small tail read of one file).
/// Costs nothing when idle, since the tick is disabled unless an app is active.
const PROGRESS_TICK_SECS: u64 = 2;

/// Quantisation applied to the published rows so fields that would otherwise
/// change every tick settle instead. Each is well below dashboard resolution:
/// 16 MB against tens of GB, and 10 s against minutes.
const QUANTISE_BYTES: u64 = 16 * 1024 * 1024;
/// Ticks between forced manifest rescans while a download is active. At a 2s
/// tick that is every 60s, which retires a row if Steam dies without ever
/// rewriting the manifest.
const TICKS_PER_RESCAN: u32 = 30;

/// How much of the tail of content_log.txt to replay on startup. Enough to catch
/// the most recent anchor without parsing a log that can run to many megabytes.
const LOG_PRIME_TAIL_BYTES: u64 = 512 * 1024;

/// Message from the filesystem-watcher callback to the sensor loop.
enum FsMsg {
    /// An ACF manifest changed - rescan the updating set.
    Changed,
    /// The watch backend reported an error. A ReadDirectoryChangesW buffer
    /// overflow (large multi-game download) can silently kill the event stream,
    /// so this asks the loop to recreate the watcher and force a rescan.
    WatcherError,
}

// Steam appmanifest StateFlags bits (Valve EAppState). A game is settled/current
// iff StateFlags == FullyInstalled (0x4); any bit in STATE_UPDATE_MASK below
// means an install/update/download/verify is pending or in progress.
//
// Canonical EAppState, verified against live client logs:
//   0x1    Uninstalled      0x100   UpdateRunning
//   0x2    UpdateRequired   0x200   UpdatePaused
//   0x4    FullyInstalled   0x400   UpdateStarted
//   0x8    UpdateQueued     0x800   Uninstalling
//   0x10   UpdateOptional   0x1000  BackupRunning
//   0x20   FilesMissing     0x2000  AppRunning
//   0x40   SharedOnly       0x4000  ComponentInUse
//   0x80   FilesCorrupt     0x8000  MovingFolder
//                           0x10000 Terminating
//
// Downloading/Staging/Committing/Preallocating/Validating are NOT StateFlags bits.
// They are a separate substate enum that appears only on `update changed :` lines
// in content_log.txt, never in the manifest, so masking for them was dead code.
// Corroborated live: an actively downloading app reads StateFlags=1026
// (UpdateStarted|UpdateRequired) with no high bits set.
#[allow(dead_code)] // documents the settled bit; used in tests
const STATE_FULLY_INSTALLED: u32 = 0x4;
const STATE_UPDATE_REQUIRED: u32 = 0x2;
const STATE_UPDATE_QUEUED: u32 = 0x8;
const STATE_UPDATE_RUNNING: u32 = 0x100;
const STATE_UPDATE_PAUSED: u32 = 0x200;
const STATE_UPDATE_STARTED: u32 = 0x400;
#[allow(dead_code)] // documents the uninstall bit, deliberately EXCLUDED from the mask
const STATE_UNINSTALLING: u32 = 0x800;

// Uninstalling (0x800), BackupRunning (0x1000), AppRunning (0x2000), Terminating
// (0x1_0000), SharedOnly (0x40), UpdateOptional (0x10) and FullyInstalled (0x4)
// are intentionally excluded so a game that is merely installed, running, backing
// up, closing, or uninstalling is never flagged "updating".
const STATE_UPDATE_MASK: u32 = STATE_UPDATE_REQUIRED // 0x2   UpdateRequired
    | STATE_UPDATE_QUEUED   // 0x8   UpdateQueued (queued counts as updating)
    | 0x20                  // FilesMissing (needs repair download)
    | 0x80                  // FilesCorrupt (needs repair download)
    | STATE_UPDATE_RUNNING  // 0x100 UpdateRunning
    | STATE_UPDATE_PAUSED   // 0x200 UpdatePaused
    | STATE_UPDATE_STARTED; // 0x400 UpdateStarted

#[derive(Debug, Clone, Default)]
struct GameUpdateState {
    app_id: String,
    name: String,
    state_flags: u32,
    /// Source manifest, e.g. `<lib>/steamapps/appmanifest_<id>.acf`. Kept for
    /// diagnostics and to identify which library an app lives in.
    #[allow(dead_code)]
    manifest_path: PathBuf,
    /// Manifest byte checkpoints. These advance mid-download but at irregular
    /// intervals (observed 472MB/28s up to 4.4GB/167s), which is why they are a
    /// fallback and an anchor rather than the live progress source. They ARE
    /// authoritative for a paused app: Steam flushes true counts on pause.
    bytes_downloaded: u64,
    bytes_to_download: u64,
    bytes_staged: u64,
    bytes_to_stage: u64,
    /// `HKCU\\Software\\Valve\\Steam\\Apps\\<appid>\\Updating`, when present.
    ///
    /// This is the ONLY reliable active-vs-paused discriminator. StateFlags keeps
    /// the UpdateStarted bit set while a download is suspended and does NOT set
    /// UpdatePaused on a user pause, so the manifest alone reports a paused
    /// download as still running. `None` means the key was absent, in which case
    /// we fall back to the StateFlags reading rather than guessing.
    registry_updating: Option<bool>,
}

impl GameUpdateState {
    /// Whether Steam is actively working this app right now, as opposed to it
    /// sitting queued or paused behind another download. Only the active app
    /// gets the live estimate; everything else keeps its exact checkpoint.
    fn is_active(&self) -> bool {
        self.state_flags & STATE_UPDATE_PAUSED == 0
            && self.registry_updating != Some(false)
            && self.state_flags & (STATE_UPDATE_RUNNING | STATE_UPDATE_STARTED) != 0
    }

    /// Base phase label, from the state bits plus the byte counters.
    ///
    /// This is the fallback and the sort key. For an actively downloading app the
    /// estimator overrides it with Steam's own word from the log, which is more
    /// specific: the substates (Downloading/Staging/Committing/Validating) are NOT
    /// StateFlags bits and appear only on `App update changed :` lines.
    fn phase(&self) -> &'static str {
        // "Paused" means an update that WAS started and is now suspended. The
        // registry flag alone is not enough: an app with an available update that
        // was never started also reads Updating=0, and that is "pending".
        let started = self.state_flags & (STATE_UPDATE_RUNNING | STATE_UPDATE_STARTED) != 0;
        if self.state_flags & STATE_UPDATE_PAUSED != 0
            || (started && self.registry_updating == Some(false))
        {
            return "paused";
        }
        if !self.is_active() {
            return if self.state_flags & STATE_UPDATE_QUEUED != 0 {
                "queued"
            } else {
                "pending"
            };
        }
        if self.bytes_to_download > 0 && self.bytes_downloaded < self.bytes_to_download {
            "downloading"
        } else if self.bytes_to_stage > 0 && self.bytes_staged < self.bytes_to_stage {
            "staging"
        } else {
            "working"
        }
    }

    /// Progress from the manifest alone, as (done, total).
    ///
    /// DOWNLOAD basis, deliberately. Two reasons. The live estimator is on the
    /// download basis, so anything else would change the meaning of `percent`
    /// whenever a row switched between the two sources: on one measured patch the
    /// stage total was 120.8 GB against a download total of 14.6 GB, a factor of
    /// 8.3. And the stage counters are not trustworthy anyway, since a real
    /// install finished with BytesStaged at 39% of BytesToStage and then stopped.
    /// Stage remains only as a fallback for a manifest with no download totals.
    fn checkpoint_bytes(&self) -> (u64, u64) {
        if self.bytes_to_download > 0 {
            (
                self.bytes_downloaded.min(self.bytes_to_download),
                self.bytes_to_download,
            )
        } else if self.bytes_to_stage > 0 {
            (
                self.bytes_staged.min(self.bytes_to_stage),
                self.bytes_to_stage,
            )
        } else {
            (0, 0)
        }
    }
}

pub struct SteamSensor {
    state: Arc<AppState>,
    library_folders: Vec<PathBuf>,
    updating_games: HashMap<String, GameUpdateState>,
    /// Cache of ACF file paths → (mtime, parsed state) to skip unchanged files
    acf_cache: HashMap<PathBuf, (std::time::SystemTime, Option<GameUpdateState>)>,
    /// Live percentage estimator, fed from content_log.txt.
    estimator: Option<DownloadEstimator>,
    /// Last `steam_downloads` payload actually sent, so an unchanged one is not
    /// republished. Without this, a download that stalls while still flagged
    /// running republishes a byte-identical payload every tick indefinitely, and
    /// every tick of genuine progress writes a Home Assistant recorder row.
    /// Mirrors the republish-only-on-change rule `hwinfo::diag_flush_plan` already
    /// applies to the other retained attribute topics.
    last_downloads: Option<(String, String)>,
}

impl SteamSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            library_folders: Vec::new(),
            updating_games: HashMap::new(),
            acf_cache: HashMap::new(),
            estimator: None,
            last_downloads: None,
        }
    }

    /// Pull new lines from content_log.txt and advance the live estimate.
    ///
    /// Cheap: one small tail read of one file, plus arithmetic. Nothing here
    /// scales with the size of the download, which is the main reason this
    /// replaced the staging-tree walk it used to do.
    async fn sample_progress(&mut self) {
        let Some(est) = self.estimator.as_mut() else {
            return;
        };

        let path = est.log_path().to_path_buf();
        let offset = est.offset();
        let (new_offset, events) =
            tokio::task::spawn_blocking(move || steam_progress::read_new_events(&path, offset))
                .await
                .unwrap_or((offset, Vec::new()));

        for ev in events {
            est.apply(ev);
        }
        est.set_offset(new_offset);
        est.advance();

        // The manifest is exact but rare. Whenever it has moved, pull the estimate
        // onto it so drift accumulated since the last flush is erased.
        for g in self.updating_games.values() {
            if g.bytes_to_download > 0 {
                est.snap_to(&g.app_id, g.bytes_downloaded, g.bytes_to_download);
            }
        }
    }

    /// Seek the estimator to the end of the log, replaying only the recent tail so
    /// a restart mid-download still recovers an anchor and a denominator.
    async fn prime_estimator(&mut self) {
        // find_steam_path reads the registry and stats directories, so it belongs
        // off the runtime with the rest of the blocking work in this file.
        let Some((root, len, events)) = tokio::task::spawn_blocking(|| {
            let root = crate::steam::find_steam_path()?;
            let path = root.join("logs").join("content_log.txt");
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let from = len.saturating_sub(LOG_PRIME_TAIL_BYTES);
            let (_, evs) = steam_progress::read_new_events(&path, from);
            Some((root, len, evs))
        })
        .await
        .ok()
        .flatten() else {
            return;
        };
        let mut est = DownloadEstimator::new(&root);

        // Only anchors and completions matter for recovery; a stale rate reading
        // from before a restart would otherwise integrate from nowhere.
        let recent: Vec<LogEvent> = events
            .into_iter()
            .filter(|e| matches!(e, LogEvent::Anchor(..) | LogEvent::Finished(..)))
            .collect();
        est.prime(len, &recent);
        self.estimator = Some(est);
    }

    /// Stable signature of the current updating set (sorted (app_id, name)
    /// pairs), used to publish only when the observable state changes. Includes
    /// the name so a manifest that gains its real name mid-update republishes.
    fn updating_signature(&self) -> Vec<(String, String)> {
        let mut sig: Vec<(String, String)> = self
            .updating_games
            .values()
            .map(|g| (g.app_id.clone(), g.name.clone()))
            .collect();
        sig.sort();
        sig
    }

    // Pre-existing offender from before the cognitive_complexity gate; new code
    // must stay under the threshold in clippy.toml.
    #[allow(clippy::cognitive_complexity)]
    pub async fn run(mut self) {
        let config = self.state.config.read().await;
        let base_interval = config.intervals.steam_check.max(10);
        drop(config);

        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

        // Discover Steam library folders (blocking registry/file I/O - off-runtime)
        self.library_folders = tokio::task::spawn_blocking(discover_library_folders_blocking)
            .await
            .unwrap_or_default();

        if self.library_folders.is_empty() {
            warn!("No Steam library folders found, steam sensor disabled");
            // Publish initial "off" state
            self.state
                .mqtt
                .publish_sensor_retained("steam_updating", "off")
                .await;
            return;
        }

        info!(
            "Found {} Steam library folder(s)",
            self.library_folders.len()
        );

        // Steam rewrites the appmanifest atomically (temp file + rename), so a
        // DIRECTORY watch catches every rewrite as a discrete event. Note it does
        // NOT only write on state transitions: BytesDownloaded/BytesStaged also
        // advance mid-download, just at irregular intervals (observed deltas from
        // 472MB/28s to 4.4GB/167s), which is why a byte-level progress feature
        // cannot rely on this cadence alone.
        // We stay fully event-driven; the timer branch below is only a
        // fallback for when the watcher can't be created at all. We do NOT tie the
        // watch to steam.exe lifetime because steamservice.exe/steamcmd.exe can
        // rewrite manifests independently.
        let (fs_tx, mut fs_rx) = mpsc::channel::<FsMsg>(16);
        // Held for its lifetime (dropping it stops the watch). Mutable so a watch
        // error can drop and recreate it; if recreation fails, using_watcher flips
        // to false and the poll fallback below takes over dynamically.
        let mut watcher = self.setup_fs_watcher(&fs_tx);
        let mut using_watcher = watcher.is_some();
        let mut watcher_error_logged = false;

        if using_watcher {
            info!("Steam sensor using filesystem watcher (event-driven)");
        } else {
            warn!(
                "Steam sensor: watcher unavailable, falling back to polling every {}s",
                base_interval
            );
        }

        // Seek the log estimator to the end before the first scan, replaying only
        // the recent tail so a restart mid-download recovers its anchor.
        self.prime_estimator().await;

        // Initial scan + unconditional publish to seed the retained state.
        self.do_full_scan().await;
        self.sample_progress().await;
        self.publish_state().await;
        let mut last_sig = self.updating_signature();
        let mut has_active = self.updating_games.values().any(|g| g.is_active());
        let mut ticks_since_scan: u32 = 0;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Steam sensor shutting down");
                    break;
                }
                // MQTT reconnect: force one republish (a broker restart can drop
                // the retained state). Treat Lagged like a reconnect.
                r = reconnect_rx.recv() => {
                    if matches!(
                        r,
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) {
                        self.do_full_scan().await;
                        self.sample_progress().await;
                        self.publish_state().await;
                        last_sig = self.updating_signature();
                        has_active = self.updating_games.values().any(|g| g.is_active());
                    }
                }
                // ACF change or watch error: re-read ground truth, publish only if
                // the updating set actually changed (skip the redundant republish).
                Some(msg) = fs_rx.recv() => {
                    // Steam's atomic temp+rename emits more than one event per
                    // manifest rewrite, and anything touching many manifests at
                    // once (library scan, bulk LastPlayed update) can fill the
                    // channel. Drain it so one logical change costs one scan
                    // instead of up to `capacity` back-to-back full scans.
                    let mut msg = msg;
                    while let Ok(extra) = fs_rx.try_recv() {
                        // An error anywhere in the burst must win: it means the
                        // watcher needs recreating regardless of the other events.
                        if matches!(extra, FsMsg::WatcherError) {
                            msg = FsMsg::WatcherError;
                        }
                    }
                    match msg {
                        FsMsg::Changed => debug!("ACF change detected, scanning"),
                        FsMsg::WatcherError => {
                            // Recreate the watcher: an overflow error means the
                            // stream is likely dead. Drop the old one FIRST so its
                            // handle is released before we re-add the watches.
                            if !watcher_error_logged {
                                warn!("Steam watcher error; recreating watcher");
                                watcher_error_logged = true;
                            }
                            drop(watcher.take());
                            watcher = self.setup_fs_watcher(&fs_tx);
                            using_watcher = watcher.is_some();
                            if using_watcher {
                                watcher_error_logged = false;
                            } else {
                                warn!(
                                    "Steam watcher recreation failed, polling every {}s",
                                    base_interval
                                );
                            }
                        }
                    }
                    // Both cases force an immediate rescan to catch anything the
                    // watcher missed during the fault window.
                    self.do_full_scan().await;
                    has_active = self.updating_games.values().any(|g| g.is_active());
                    let sig = self.updating_signature();
                    if sig == last_sig {
                        // Same set of apps, but the manifest was rewritten, so the
                        // byte checkpoints moved. Republish the queue without the
                        // full steam_updating churn.
                        self.publish_downloads().await;
                    } else {
                        self.sample_progress().await;
                        self.publish_state().await;
                        last_sig = sig;
                    }
                }
                // Live progress tick. Gated on something actually being written, so
                // idle cost is exactly zero: with no active download this branch is
                // disabled and the sensor stays fully event-driven. The manifest
                // flushes at wildly irregular intervals (472MB/28s up to 4.4GB/167s
                // observed), so this is what makes the bar move smoothly in between.
                () = tokio::time::sleep(Duration::from_secs(PROGRESS_TICK_SECS)), if has_active => {
                    self.sample_progress().await;
                    // Re-read ground truth occasionally. If Steam is killed
                    // mid-download the manifest is never rewritten, so no watcher
                    // event ever arrives and nothing would otherwise retire the
                    // row or stop this tick.
                    ticks_since_scan += 1;
                    if ticks_since_scan >= TICKS_PER_RESCAN {
                        ticks_since_scan = 0;
                        self.do_full_scan().await;
                        has_active = self.updating_games.values().any(|g| g.is_active());
                    }
                    self.publish_downloads().await;
                }
                // Fallback poll: only ever fires when the watcher couldn't be
                // created (the `if` disables this branch when a watcher is live).
                () = tokio::time::sleep(Duration::from_secs(base_interval)), if !using_watcher => {
                    self.do_full_scan().await;
                    has_active = self.updating_games.values().any(|g| g.is_active());
                    let sig = self.updating_signature();
                    if sig != last_sig {
                        self.sample_progress().await;
                        self.publish_state().await;
                        last_sig = sig;
                    }
                }
            }
        }
    }

    /// Set up a filesystem watcher on all Steam library folders.
    /// Returns the watcher handle (must be kept alive) or None if setup failed.
    fn setup_fs_watcher(&self, fs_tx: &mpsc::Sender<FsMsg>) -> Option<notify::RecommendedWatcher> {
        use notify::{Event, EventKind, RecursiveMode, Watcher};

        let tx = fs_tx.clone();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        // Only react to ACF file modifications/creations
                        let is_acf_change = matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                        ) && event.paths.iter().any(|p| {
                            p.extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("acf"))
                        });

                        if is_acf_change {
                            // Non-blocking send - if full, skip (caught next scan).
                            let _ = tx.try_send(FsMsg::Changed);
                        }
                    }
                    Err(_) => {
                        // A backend error (notably a ReadDirectoryChangesW buffer
                        // overflow) can silently kill the stream; ask the loop to
                        // recreate the watcher rather than freeze forever.
                        let _ = tx.try_send(FsMsg::WatcherError);
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    warn!("Failed to create filesystem watcher: {}", e);
                    return None;
                }
            };

        // Watch each Steam library folder (non-recursive - ACF files are at the top level)
        let mut watched_any = false;
        for folder in &self.library_folders {
            if let Err(e) = watcher.watch(folder, RecursiveMode::NonRecursive) {
                warn!("Failed to watch {:?}: {}", folder, e);
            } else {
                debug!("Watching Steam library: {:?}", folder);
                watched_any = true;
            }
        }

        if watched_any { Some(watcher) } else { None }
    }

    // (library discovery moved to the free blocking functions below so it can run
    // off the single-threaded runtime via spawn_blocking)

    async fn do_full_scan(&mut self) {
        // Move blocking filesystem I/O off the single-threaded async runtime
        let library_folders = self.library_folders.clone();
        let mut acf_cache = std::mem::take(&mut self.acf_cache);

        let (new_updating, returned_cache) = tokio::task::spawn_blocking(move || {
            let mut updating: HashMap<String, GameUpdateState> = HashMap::new();
            let mut seen_paths: HashSet<PathBuf> = HashSet::new();

            for lib_folder in &library_folders {
                let entries = match std::fs::read_dir(lib_folder) {
                    Ok(e) => e,
                    Err(e) => {
                        error!("Failed to read steam library dir {:?}: {}", lib_folder, e);
                        continue;
                    }
                };

                for dir_entry in entries.flatten() {
                    // Both of these come straight out of the WIN32_FIND_DATAW that
                    // FindNextFileW already returned, so they cost zero syscalls.
                    // `path.is_file()` and `fs::metadata(&path)` would each be a
                    // full open instead: two wasted syscalls per manifest, on a
                    // path that runs on every game launch and exit.
                    if !dir_entry.file_type().is_ok_and(|t| t.is_file()) {
                        continue;
                    }
                    let path = dir_entry.path();
                    let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !fname.starts_with("appmanifest_") || !fname.ends_with(".acf") {
                        continue;
                    }

                    seen_paths.insert(path.clone());

                    let mtime = dir_entry.metadata().and_then(|m| m.modified()).ok();

                    // Borrow the cached entry rather than cloning it: most
                    // manifests belong to settled games and are dropped by the
                    // is_updating check below, so cloning first would allocate for
                    // every installed game on every scan.
                    if let Some(mt) = mtime
                        && let Some((cached_mt, cached_state)) = acf_cache.get(&path)
                        && *cached_mt == mt
                    {
                        if let Some(gs) = cached_state.as_ref().filter(|g| is_updating(g)) {
                            let mut gs = gs.clone();
                            // Always re-read: the cache is keyed on manifest mtime,
                            // but pausing flips this flag WITHOUT rewriting the
                            // manifest, so a cached entry would go stale exactly
                            // when it matters.
                            gs.registry_updating = registry_updating_flag(&gs.app_id);
                            updating.insert(gs.app_id.clone(), gs);
                        }
                        continue;
                    }

                    let state = parse_acf_file(&path);
                    if let Some(mt) = mtime {
                        acf_cache.insert(path.clone(), (mt, state.clone()));
                    }
                    if let Some(mut gs) = state
                        && is_updating(&gs)
                    {
                        gs.registry_updating = registry_updating_flag(&gs.app_id);
                        updating.insert(gs.app_id.clone(), gs);
                    }
                }
            }

            acf_cache.retain(|path, _| seen_paths.contains(path));
            (updating, acf_cache)
        })
        .await
        .unwrap_or_else(|e| {
            warn!("Steam full scan task panicked: {e}");
            Default::default()
        });

        self.acf_cache = returned_cache;

        // Check for changes
        let was_updating = !self.updating_games.is_empty();
        let is_updating_now = !new_updating.is_empty();

        // Log changes
        if was_updating != is_updating_now || self.updating_games.len() != new_updating.len() {
            if is_updating_now {
                let names: Vec<&str> = new_updating.values().map(|g| g.name.as_str()).collect();
                info!("Steam games updating: {:?}", names);
            } else if was_updating {
                info!("Steam updates completed");
            }
        }

        self.updating_games = new_updating;
    }
}

/// Parse an ACF manifest file into a GameUpdateState
fn parse_acf_file(path: &Path) -> Option<GameUpdateState> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    parse_acf_content(&content, path)
}

/// Parse ACF content string into a GameUpdateState (testable without filesystem)
fn parse_acf_content(content: &str, manifest_path: &Path) -> Option<GameUpdateState> {
    let mut app_id = String::new();
    let mut name = String::new();
    let mut state_flags: u32 = 0;
    let mut bytes_downloaded: u64 = 0;
    let mut bytes_to_download: u64 = 0;
    let mut bytes_staged: u64 = 0;
    let mut bytes_to_stage: u64 = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("\"appid\"") || trimmed.starts_with("\"AppID\"") {
            if let Some(val) = extract_vdf_value(trimmed) {
                app_id = val;
            }
        } else if trimmed.starts_with("\"name\"") {
            if let Some(val) = extract_vdf_value(trimmed) {
                name = val;
            }
        } else if (trimmed.starts_with("\"StateFlags\"") || trimmed.starts_with("\"stateflags\""))
            && let Some(val) = extract_vdf_value(trimmed)
        {
            state_flags = val.parse().unwrap_or(0);
        } else if let Some(target) = byte_counter_slot(
            trimmed,
            &mut bytes_downloaded,
            &mut bytes_to_download,
            &mut bytes_staged,
            &mut bytes_to_stage,
        ) && let Some(val) = extract_vdf_value(trimmed)
        {
            *target = val.parse().unwrap_or(0);
        }
    }

    if app_id.is_empty() {
        return None;
    }

    Some(GameUpdateState {
        app_id,
        name,
        state_flags,
        manifest_path: manifest_path.to_path_buf(),
        bytes_downloaded,
        bytes_to_download,
        bytes_staged,
        bytes_to_stage,
        // Stamped on by the caller after parsing: this comes from the registry,
        // not the manifest, and is re-read on every scan because pausing flips it
        // without rewriting the manifest.
        registry_updating: None,
    })
}

/// Match a manifest line against the four byte counters, returning the slot to
/// write. Keys are matched case-insensitively because Steam has shipped both
/// casings over time (the same reason StateFlags is checked twice above).
fn byte_counter_slot<'a>(
    line: &str,
    downloaded: &'a mut u64,
    to_download: &'a mut u64,
    staged: &'a mut u64,
    to_stage: &'a mut u64,
) -> Option<&'a mut u64> {
    let key = line.split('"').nth(1)?;
    // Longest-first so "BytesToDownload" is not shadowed by "BytesDownloaded".
    if key.eq_ignore_ascii_case("BytesToDownload") {
        Some(to_download)
    } else if key.eq_ignore_ascii_case("BytesDownloaded") {
        Some(downloaded)
    } else if key.eq_ignore_ascii_case("BytesToStage") {
        Some(to_stage)
    } else if key.eq_ignore_ascii_case("BytesStaged") {
        Some(staged)
    } else {
        None
    }
}

/// Read `HKCU\\Software\\Valve\\Steam\\Apps\\<appid>\\Updating`.
///
/// `Some(true)` while Steam is actively transferring, `Some(false)` when it is
/// not, `None` if the key is absent. This is the only signal that distinguishes a
/// paused download from a running one: the appmanifest keeps its UpdateStarted
/// bit set while suspended and does not set UpdatePaused on a user pause.
/// Blocking registry I/O, so it runs inside the same spawn_blocking as the scan.
#[cfg(windows)]
fn registry_updating_flag(app_id: &str) -> Option<bool> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey(format!("Software\\Valve\\Steam\\Apps\\{app_id}"))
        .ok()?;
    let v: u32 = key.get_value("Updating").ok()?;
    Some(v != 0)
}

#[cfg(not(windows))]
fn registry_updating_flag(_app_id: &str) -> Option<bool> {
    None
}

/// Extract a quoted value from a VDF key-value line
/// Format: "key"\t\t"value"
fn extract_vdf_value(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.split('"').collect();
    if parts.len() >= 4 {
        Some(parts[3].to_string())
    } else {
        None
    }
}

/// Check if a game's state flags indicate an update in progress.
fn is_updating(game: &GameUpdateState) -> bool {
    game.state_flags & STATE_UPDATE_MASK != 0
}

impl SteamSensor {
    async fn publish_state(&mut self) {
        let is_updating = !self.updating_games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };

        self.state
            .mqtt
            .publish_sensor_retained("steam_updating", state_str)
            .await;

        // Publish attributes with game names
        if is_updating {
            // Sorted because `updating_games` is rebuilt as a fresh HashMap on
            // every scan, so its iteration order changes run to run. Home
            // Assistant compares attribute lists element-wise, and an unsorted
            // list would publish a spurious state change (and a recorder row)
            // whenever the order shuffled with nothing underneath it changing.
            let mut names: Vec<&str> = self
                .updating_games
                .values()
                .map(|g| g.name.as_str())
                .collect();
            names.sort_unstable();
            let attrs = serde_json::json!({
                "updating_games": names,
                "count": self.updating_games.len()
            });
            self.state
                .mqtt
                .publish_sensor_attributes("steam_updating", &attrs)
                .await;
        } else {
            self.state
                .mqtt
                .publish_sensor_attributes("steam_updating", &IDLE_STEAM_ATTRS)
                .await;
        }

        self.publish_downloads().await;
    }

    /// Publish the full download queue as `steam_downloads`: state is the number
    /// of apps in the pipeline, and the `games` attribute is one row per app.
    ///
    /// The attribute shape is fixed by the existing Home Assistant dashboard,
    /// which iterates `state_attr('sensor.steam_downloads','games')` and reads
    /// name/state/percent/eta/bytes_rate/bytes_done/bytes_total/appid per row.
    /// Do not rename these keys without updating the dashboard template.
    async fn publish_downloads(&mut self) {
        // Sort the source structs BEFORE building JSON, rather than re-extracting
        // "state" and "name" back out of Values we just constructed. Ordering is
        // stable so the dashboard grid does not reshuffle between publishes: the
        // app being worked on first, then alphabetically.
        let mut ordered: Vec<&GameUpdateState> = self.updating_games.values().collect();
        ordered.sort_by(|a, b| {
            let rank = |g: &GameUpdateState| match g.phase() {
                "downloading" | "staging" | "working" => 0,
                "paused" => 1,
                "queued" => 2,
                _ => 3,
            };
            rank(a).cmp(&rank(b)).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });

        let mut games: Vec<serde_json::Value> = Vec::with_capacity(ordered.len());

        for g in ordered {
            let (mut done, total) = g.checkpoint_bytes();
            let mut percent = if total > 0 {
                (done as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            let mut phase = g.phase();

            // The live estimate wins while a download is running: the manifest is
            // exact but is written so rarely that on a real 112 GB install it sat
            // frozen for over three minutes and then jumped straight to 100%.
            // Only for an ACTIVE app, so a paused or queued row keeps its exact
            // checkpoint rather than a stale estimate.
            if g.is_active()
                && let Some(est) = self.estimator.as_ref()
                && let Some(live) = est.percent_for(&g.app_id)
            {
                percent = live;
                done = ((live / 100.0) * total as f64) as u64;
                // Steam names its own phase in the log, which beats inferring it
                // from byte counters.
                if let Some(p) = est.phase_for(&g.app_id) {
                    phase = p;
                }
            }

            // Quantise every field that would otherwise move on every single tick.
            // Raw byte counts and a raw ETA change constantly and force a publish
            // (and a recorder row) even when nothing meaningful happened; at these
            // magnitudes the rounding is invisible on a dashboard.
            games.push(serde_json::json!({
                "appid": g.app_id.parse::<u64>().unwrap_or(0),
                "name": g.name,
                "state": phase,
                "percent": (percent * 10.0).round() / 10.0,   // 0.1%
                "bytes_done": done / QUANTISE_BYTES * QUANTISE_BYTES,
                "bytes_total": total,
                // Steam's log reports a transfer rate but attributes it to no app, so
                // there is no honest per-app figure to publish. The dashboard reads
                // these keys, so they stay present rather than breaking its template.
                "bytes_rate": 0,
                "eta": 0,
            }));
        }

        let state_str = games.len().to_string();
        let attrs = serde_json::json!({ "games": games });
        let Ok(attrs_str) = serde_json::to_string(&attrs) else {
            warn!("Failed to serialize steam_downloads attributes");
            return;
        };

        // Republish only on change. A stalled-but-still-running download would
        // otherwise resend an identical retained QoS-1 payload every tick forever.
        if self
            .last_downloads
            .as_ref()
            .is_some_and(|(s, a)| *s == state_str && *a == attrs_str)
        {
            return;
        }

        self.state
            .mqtt
            .publish_sensor_retained("steam_downloads", &state_str)
            .await;
        self.state
            .mqtt
            .publish_sensor_attributes("steam_downloads", &attrs)
            .await;
        self.last_downloads = Some((state_str, attrs_str));
    }
}

/// Discover Steam library folders. Does blocking I/O (registry read on Windows,
/// file reads), so run it via spawn_blocking, not directly on the runtime.
fn discover_library_folders_blocking() -> Vec<PathBuf> {
    let mut folders = Vec::new();

    let Some(steam_path) = crate::steam::find_steam_path() else {
        debug!("Steam installation not found");
        return folders;
    };

    // Primary library is in the Steam install dir.
    let primary_steamapps = steam_path.join("steamapps");
    if primary_steamapps.exists() {
        folders.push(primary_steamapps.clone());
    }

    // Additional libraries from libraryfolders.vdf.
    let vdf_path = primary_steamapps.join("libraryfolders.vdf");
    if vdf_path.exists()
        && let Ok(content) = std::fs::read_to_string(&vdf_path)
    {
        parse_library_folders_vdf(&content, &mut folders);
    }
    folders
}

fn parse_library_folders_vdf(content: &str, folders: &mut Vec<PathBuf>) {
    // Format: "path"		"D:\\SteamLibrary"
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("\"path\"") {
            let parts: Vec<&str> = trimmed.split('"').collect();
            if parts.len() >= 4 {
                let path_str = crate::steam::vdf::unescape_vdf(parts[3]);
                let library_path = PathBuf::from(&path_str).join("steamapps");
                if library_path.exists() && !folders.contains(&library_path) {
                    folders.push(library_path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_acf_content_byte_counters() {
        // Real-shaped manifest for an active download. "BytesToDownload" must not
        // be swallowed by the "BytesDownloaded" match, which is why the key test is
        // longest-first and exact rather than a prefix check.
        let content = r#"
"AppState"
{
	"appid"		"1086940"
	"name"		"Baldur's Gate 3"
	"StateFlags"		"1026"
	"BytesDownloaded"		"4241500000"
	"BytesToDownload"		"127613000000"
	"BytesStaged"		"4681000000"
	"BytesToStage"		"155340000000"
}
"#;
        let g = parse_acf_content(content, Path::new("/lib/steamapps/appmanifest_1086940.acf"))
            .expect("manifest parses");
        assert_eq!(g.bytes_downloaded, 4_241_500_000);
        assert_eq!(g.bytes_to_download, 127_613_000_000);
        assert_eq!(g.bytes_staged, 4_681_000_000);
        assert_eq!(g.bytes_to_stage, 155_340_000_000);
        assert_eq!(g.phase(), "downloading");
    }

    #[test]
    fn test_parse_acf_content_missing_counters_default_zero() {
        // Older or freshly-created manifests omit the byte keys entirely.
        let content = r#"
"AppState"
{
	"appid"		"440"
	"name"		"Team Fortress 2"
	"StateFlags"		"4"
}
"#;
        let g = parse_acf_content(content, Path::new("/lib/steamapps/appmanifest_440.acf"))
            .expect("manifest parses");
        assert_eq!(g.bytes_to_stage, 0);
        assert_eq!(g.checkpoint_bytes(), (0, 0));
    }

    #[test]
    fn test_byte_counter_slot_distinguishes_similar_keys() {
        let (mut dl, mut to_dl, mut st, mut to_st) = (0u64, 0u64, 0u64, 0u64);
        {
            let slot = byte_counter_slot(
                "\"BytesToDownload\"\t\t\"5\"",
                &mut dl,
                &mut to_dl,
                &mut st,
                &mut to_st,
            );
            *slot.expect("matched") = 5;
        }
        assert_eq!(to_dl, 5);
        assert_eq!(
            dl, 0,
            "BytesToDownload must not write the BytesDownloaded slot"
        );

        let (mut dl2, mut to_dl2, mut st2, mut to_st2) = (0u64, 0u64, 0u64, 0u64);
        {
            let slot = byte_counter_slot(
                "\"BytesToStage\"\t\t\"7\"",
                &mut dl2,
                &mut to_dl2,
                &mut st2,
                &mut to_st2,
            );
            *slot.expect("matched") = 7;
        }
        assert_eq!(to_st2, 7);
        assert_eq!(st2, 0, "BytesToStage must not write the BytesStaged slot");
    }

    #[test]
    fn test_byte_counter_slot_ignores_unrelated_keys() {
        let (mut a, mut b, mut c, mut d) = (0u64, 0u64, 0u64, 0u64);
        assert!(
            byte_counter_slot("\"StateFlags\"\t\t\"4\"", &mut a, &mut b, &mut c, &mut d).is_none()
        );
        assert!(byte_counter_slot("not a vdf line", &mut a, &mut b, &mut c, &mut d).is_none());
    }

    // -- extract_vdf_value tests --

    #[test]
    fn test_extract_vdf_value_basic() {
        let line = r#"	"appid"		"730""#;
        assert_eq!(extract_vdf_value(line), Some("730".to_string()));
    }

    #[test]
    fn test_extract_vdf_value_name() {
        let line = r#"	"name"		"Counter-Strike 2""#;
        assert_eq!(
            extract_vdf_value(line),
            Some("Counter-Strike 2".to_string())
        );
    }

    #[test]
    fn test_extract_vdf_value_no_value() {
        let line = r#"	"appid""#;
        assert_eq!(extract_vdf_value(line), None);
    }

    #[test]
    fn test_extract_vdf_value_empty_value() {
        let line = r#"	"key"		"""#;
        assert_eq!(extract_vdf_value(line), Some(String::new()));
    }

    #[test]
    fn test_extract_vdf_value_state_flags() {
        let line = r#"	"StateFlags"		"4""#;
        assert_eq!(extract_vdf_value(line), Some("4".to_string()));
    }

    // -- is_updating tests --

    fn make_game(flags: u32) -> GameUpdateState {
        GameUpdateState {
            app_id: "730".to_string(),
            name: "Test Game".to_string(),
            state_flags: flags,
            manifest_path: PathBuf::from("/tmp/test.acf"),
            ..Default::default()
        }
    }

    #[test]
    fn test_is_updating_fully_installed() {
        assert!(!is_updating(&make_game(STATE_FULLY_INSTALLED)));
    }

    #[test]
    fn test_is_updating_zero_flags() {
        assert!(!is_updating(&make_game(0)));
    }

    #[test]
    fn test_is_updating_update_running() {
        assert!(is_updating(&make_game(STATE_UPDATE_RUNNING)));
    }

    #[test]
    fn test_is_updating_update_paused() {
        assert!(is_updating(&make_game(STATE_UPDATE_PAUSED)));
    }

    #[test]
    fn test_is_updating_queued() {
        // 0x8 = UpdateQueued. Steam sets this on every app waiting behind the
        // active download, and it MUST read as updating: the mask used to omit
        // it, so a queued download reported `steam_updating = off`.
        assert!(is_updating(&make_game(STATE_UPDATE_QUEUED)));
        // Observed live as `state changed : Fully Installed,Update Queued`
        // paired with state 0xc.
        assert!(is_updating(&make_game(
            STATE_FULLY_INSTALLED | STATE_UPDATE_QUEUED
        )));
    }

    #[test]
    fn test_is_updating_active_download_flags() {
        // Steam writes 1026 (UpdateStarted|UpdateRequired) for an app that is
        // actively downloading. There is no "Downloading" StateFlags bit.
        assert!(is_updating(&make_game(1026)));
    }

    #[test]
    fn test_is_updating_combined_flags() {
        assert!(is_updating(&make_game(
            STATE_FULLY_INSTALLED | STATE_UPDATE_RUNNING
        )));
    }

    #[test]
    fn test_is_updating_update_required() {
        // 2 = UpdateRequired is a real update bit.
        assert!(is_updating(&make_game(2)));
    }

    #[test]
    fn test_is_updating_running_game_not_updating() {
        // Installed(0x4) + AppRunning(0x2000) must NOT read as updating (the old
        // "any non-installed flag" catch-all wrongly did).
        assert!(!is_updating(&make_game(0x4 | 0x2000)));
        assert!(!is_updating(&make_game(0x2000)));
        // SharedOnly(0x40) and UpdateOptional(0x10) alone are not updates either.
        assert!(!is_updating(&make_game(0x40)));
        assert!(!is_updating(&make_game(0x10)));
        // Terminating(0x1_0000) is a closing game, not an update. The old mask
        // wrongly included this bit, mislabeled as "Reconfiguring".
        assert!(!is_updating(&make_game(0x1_0000)));
        assert!(!is_updating(&make_game(STATE_FULLY_INSTALLED | 0x1_0000)));
    }

    #[test]
    fn test_is_updating_uninstalling_excluded() {
        // Uninstalling (0x800) must NOT read as "updating". The old mask wrongly
        // included this bit (it was mislabeled as UpdatePaused).
        assert!(!is_updating(&make_game(STATE_UNINSTALLING)));
        assert!(!is_updating(&make_game(
            STATE_FULLY_INSTALLED | STATE_UNINSTALLING
        )));
    }

    #[test]
    fn test_is_updating_paused_download() {
        // A started-but-paused update (0x602 = UpdateStarted|UpdatePaused|
        // UpdateRequired) still counts as updating.
        assert!(is_updating(&make_game(
            STATE_UPDATE_STARTED | STATE_UPDATE_PAUSED | STATE_UPDATE_REQUIRED
        )));
    }

    // -- phase / byte-counter tests --

    fn game_with(flags: u32, dl: u64, to_dl: u64, st: u64, to_st: u64) -> GameUpdateState {
        GameUpdateState {
            app_id: "1086940".to_string(),
            name: "Baldur's Gate 3".to_string(),
            state_flags: flags,
            manifest_path: PathBuf::from("/lib/steamapps/appmanifest_1086940.acf"),
            bytes_downloaded: dl,
            bytes_to_download: to_dl,
            bytes_staged: st,
            bytes_to_stage: to_st,
            registry_updating: None,
        }
    }

    #[test]
    fn phase_paused_wins_over_everything() {
        // A paused app still carries UpdateStarted; the pause bit must dominate,
        // otherwise the dashboard would show it as actively downloading.
        let g = game_with(STATE_UPDATE_STARTED | STATE_UPDATE_PAUSED, 10, 100, 10, 100);
        assert_eq!(g.phase(), "paused");
        assert!(!g.is_active());
    }

    #[test]
    fn phase_paused_from_registry_when_stateflags_lie() {
        // THE BUG THAT SHIPPED IN v3.1.0. Steam does NOT set UpdatePaused (0x200)
        // on a user pause: the manifest keeps reading 1026 (UpdateStarted|
        // UpdateRequired) exactly as it does while running. Only the registry's
        // Updating DWORD flips. Without this, a paused download displayed as
        // "downloading" forever with a frozen percentage.
        let mut g = game_with(1026, 3_950_000_000, 118_850_000_000, 0, 144_670_000_000);
        assert_eq!(g.phase(), "downloading", "sanity: running looks running");
        g.registry_updating = Some(false);
        assert_eq!(g.phase(), "paused");
        assert!(
            !g.is_active(),
            "a paused app must stop taking the live estimate too"
        );
    }

    #[test]
    fn phase_pending_not_paused_when_never_started() {
        // An app with an available update that was never started ALSO reads
        // Updating=0. It must stay "pending", not become "paused", which is why
        // the registry check is gated on the started bits.
        let mut g = game_with(STATE_UPDATE_REQUIRED, 0, 0, 0, 0);
        g.registry_updating = Some(false);
        assert_eq!(g.phase(), "pending");
    }

    #[test]
    fn phase_missing_registry_key_falls_back_to_stateflags() {
        // None means the key was absent; behaviour must be unchanged from before.
        let mut g = game_with(1026, 1, 100, 0, 0);
        g.registry_updating = None;
        assert_eq!(g.phase(), "downloading");
        assert!(g.is_active());
    }

    #[test]
    fn phase_registry_true_keeps_it_running() {
        let mut g = game_with(1026, 1, 100, 0, 0);
        g.registry_updating = Some(true);
        assert_eq!(g.phase(), "downloading");
        assert!(g.is_active());
    }

    #[test]
    fn phase_downloading_while_bytes_outstanding() {
        // 1026 = UpdateStarted|UpdateRequired, what Steam actually writes for an
        // active download (verified live 2026-07-25).
        let g = game_with(1026, 3_950_000_000, 118_850_000_000, 0, 144_670_000_000);
        assert_eq!(g.phase(), "downloading");
        assert!(g.is_active());
    }

    #[test]
    fn phase_staging_once_download_complete() {
        // The download finished but the disk work has not: this is the transition
        // the user specifically wanted visible.
        let g = game_with(
            1026,
            188_000_000,
            188_000_000,
            2_000_000_000,
            22_500_000_000,
        );
        assert_eq!(g.phase(), "staging");
    }

    #[test]
    fn phase_working_when_both_totals_met() {
        let g = game_with(1026, 100, 100, 200, 200);
        assert_eq!(g.phase(), "working");
    }

    #[test]
    fn phase_queued_is_not_active() {
        // Depends on the 0x8 fix: before it, a queued app was invisible entirely.
        let g = game_with(STATE_FULLY_INSTALLED | STATE_UPDATE_QUEUED, 0, 0, 0, 0);
        assert_eq!(g.phase(), "queued");
        assert!(!g.is_active());
    }

    #[test]
    fn phase_pending_for_update_required_only() {
        let g = game_with(STATE_UPDATE_REQUIRED, 0, 0, 0, 0);
        assert_eq!(g.phase(), "pending");
        assert!(!g.is_active());
    }

    #[test]
    fn checkpoint_prefers_download_basis() {
        // MUST match the live estimator's basis. A real patch had a stage total of
        // 120.8 GB against a download total of 14.6 GB, so preferring stage made
        // `percent` change meaning by a factor of 8.3 whenever a row switched
        // between the estimator and the manifest.
        let g = game_with(1026, 50, 100, 25, 200);
        assert_eq!(g.checkpoint_bytes(), (50, 100));
    }

    #[test]
    fn checkpoint_falls_back_to_stage_basis() {
        // Only when the manifest carries no download totals at all.
        let g = game_with(1026, 0, 0, 25, 200);
        assert_eq!(g.checkpoint_bytes(), (25, 200));
    }

    #[test]
    fn checkpoint_clamps_overshoot() {
        // Guard against a malformed manifest producing >100%.
        let g = game_with(1026, 0, 0, 500, 200);
        assert_eq!(g.checkpoint_bytes(), (200, 200));
    }

    #[test]
    fn checkpoint_zero_when_no_totals() {
        let g = game_with(1026, 0, 0, 0, 0);
        assert_eq!(g.checkpoint_bytes(), (0, 0));
    }

    #[test]
    fn quantisation_settles_fields_that_would_churn_every_tick() {
        // A stalled-but-running download must produce a byte-identical payload so
        // the change gate can suppress it. Two samples a few MB apart (well inside
        // one quantum) must land on the same published values.
        let a_done = 4_520_000_000u64;
        let b_done = a_done + 3_000_000; // 3 MB later, inside the 16 MB quantum
        assert_eq!(
            a_done / QUANTISE_BYTES * QUANTISE_BYTES,
            b_done / QUANTISE_BYTES * QUANTISE_BYTES
        );
    }

    #[test]
    fn quantisation_still_advances_on_real_progress() {
        // The gate must not be so coarse that genuine progress stops publishing.
        // One 16 MB quantum is ~0.16s at 100 MB/s, so a 5s tick always crosses it.
        let start = 4_520_000_000u64;
        let after_one_tick = start + 5 * 100 * 1024 * 1024; // 5s at 100 MB/s
        assert_ne!(
            start / QUANTISE_BYTES * QUANTISE_BYTES,
            after_one_tick / QUANTISE_BYTES * QUANTISE_BYTES
        );
    }

    #[test]
    fn updating_names_are_sorted_for_stable_attributes() {
        // updating_games is a fresh HashMap each scan, so iteration order varies
        // run to run. HA compares attribute lists element-wise, so an unsorted
        // list would publish spurious state changes.
        let mut names = vec!["Warframe", "Baldur's Gate 3", "Overwatch"];
        names.sort_unstable();
        assert_eq!(names, ["Baldur's Gate 3", "Overwatch", "Warframe"]);
    }

    // -- parse_acf_content tests --

    #[test]
    fn test_parse_acf_content_valid() {
        let content = r#"
"AppState"
{
	"appid"		"730"
	"name"		"Counter-Strike 2"
	"StateFlags"		"4"
	"installdir"		"Counter-Strike Global Offensive"
}
"#;
        let path = PathBuf::from("/tmp/appmanifest_730.acf");
        let result = parse_acf_content(content, &path).unwrap();
        assert_eq!(result.app_id, "730");
        assert_eq!(result.name, "Counter-Strike 2");
        assert_eq!(result.state_flags, 4);
    }

    #[test]
    fn test_parse_acf_content_updating() {
        let content = r#"
"AppState"
{
	"appid"		"440"
	"name"		"Team Fortress 2"
	"StateFlags"		"1028"
}
"#;
        let path = PathBuf::from("/tmp/appmanifest_440.acf");
        let result = parse_acf_content(content, &path).unwrap();
        assert_eq!(result.app_id, "440");
        assert_eq!(result.state_flags, 1028);
        assert!(is_updating(&result));
    }

    #[test]
    fn test_parse_acf_content_missing_appid() {
        let content = r#"
"AppState"
{
	"name"		"No AppID Game"
	"StateFlags"		"4"
}
"#;
        let path = PathBuf::from("/tmp/test.acf");
        assert!(parse_acf_content(content, &path).is_none());
    }

    #[test]
    fn test_parse_acf_content_empty() {
        let path = PathBuf::from("/tmp/empty.acf");
        assert!(parse_acf_content("", &path).is_none());
    }

    #[test]
    fn test_parse_acf_content_case_variants() {
        // Tests both "appid" and "AppID" variants
        let content = r#"
"AppState"
{
	"AppID"		"553850"
	"name"		"HELLDIVERS 2"
	"stateflags"		"4"
}
"#;
        let path = PathBuf::from("/tmp/test.acf");
        let result = parse_acf_content(content, &path).unwrap();
        assert_eq!(result.app_id, "553850");
        assert_eq!(result.state_flags, 4);
    }

    // ===== MQTT content verification - exact payloads for steam_updating sensor =====

    #[test]
    fn test_steam_updating_state_string_when_updating() {
        // When updating_games is non-empty, the state string is "on"
        let games = [make_game(STATE_UPDATE_RUNNING)];
        let is_updating = !games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        assert_eq!(state_str, "on");
    }

    #[test]
    fn test_steam_updating_state_string_when_idle() {
        // When no games updating, the state string is "off"
        let games: Vec<GameUpdateState> = vec![];
        let is_updating = !games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        assert_eq!(state_str, "off");
    }

    #[test]
    fn test_steam_updating_attributes_json_with_games() {
        // Exact JSON published to homeassistant/sensor/{device}/steam_updating/attributes
        let games = [
            GameUpdateState {
                app_id: "730".to_string(),
                name: "Counter-Strike 2".to_string(),
                state_flags: STATE_UPDATE_RUNNING,
                manifest_path: PathBuf::from("/tmp/appmanifest_730.acf"),
                ..Default::default()
            },
            GameUpdateState {
                app_id: "440".to_string(),
                name: "Team Fortress 2".to_string(),
                state_flags: STATE_UPDATE_QUEUED,
                manifest_path: PathBuf::from("/tmp/appmanifest_440.acf"),
                ..Default::default()
            },
        ];
        let names: Vec<&str> = games.iter().map(|g| g.name.as_str()).collect();
        let attrs = serde_json::json!({
            "updating_games": names,
            "count": games.len()
        });

        // Verify exact JSON structure
        let obj = attrs.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert_eq!(attrs["count"], 2);
        let game_names = attrs["updating_games"].as_array().unwrap();
        assert_eq!(game_names.len(), 2);
        assert_eq!(game_names[0], "Counter-Strike 2");
        assert_eq!(game_names[1], "Team Fortress 2");

        // Verify the exact serialized bytes that go to MQTT
        let json_bytes = serde_json::to_vec(&attrs).unwrap();
        let roundtrip: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(roundtrip, attrs);
    }

    #[test]
    fn test_steam_updating_attributes_json_when_idle() {
        // Exact JSON published when no games are updating
        let attrs = serde_json::json!({
            "updating_games": Vec::<String>::new(),
            "count": 0
        });
        let obj = attrs.as_object().unwrap();
        assert_eq!(attrs["count"], 0);
        assert!(attrs["updating_games"].as_array().unwrap().is_empty());
        assert_eq!(obj.len(), 2); // always has both fields even when idle
    }

    #[test]
    fn test_steam_updating_full_mqtt_payload_content() {
        // End-to-end: simulate what SteamSensor::publish_state builds
        // Scenario: one game downloading

        // 1. State topic payload (retained)
        let updating_games: HashMap<String, GameUpdateState> = {
            let mut m = HashMap::new();
            m.insert(
                "553850".to_string(),
                GameUpdateState {
                    app_id: "553850".to_string(),
                    name: "HELLDIVERS 2".to_string(),
                    state_flags: STATE_UPDATE_STARTED | STATE_UPDATE_REQUIRED,
                    manifest_path: PathBuf::from("/tmp/appmanifest_553850.acf"),
                    ..Default::default()
                },
            );
            m
        };

        let is_updating = !updating_games.is_empty();
        let state_str = if is_updating { "on" } else { "off" };
        // This exact string goes to: homeassistant/sensor/{device}/steam_updating/state
        assert_eq!(state_str, "on");

        // 2. Attributes topic payload (retained)
        let names: Vec<&str> = updating_games.values().map(|g| g.name.as_str()).collect();
        let attrs = serde_json::json!({
            "updating_games": names,
            "count": updating_games.len()
        });
        // This exact JSON goes to: homeassistant/sensor/{device}/steam_updating/attributes
        assert_eq!(attrs["count"], 1);
        assert_eq!(attrs["updating_games"][0], "HELLDIVERS 2");
    }
}
