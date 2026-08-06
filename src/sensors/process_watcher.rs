//! Event-driven process watcher using WMI
//!
//! Instead of polling for process lists, this module subscribes to Windows WMI
//! events for process creation and deletion. This provides:
//! - Immediate detection of new processes (no polling delay)
//! - Lower CPU usage (no periodic enumeration)
//! - Always up-to-date process list
//! - Push notifications to subscribers when processes change
//!
//! Falls back to polling if WMI subscription fails.

use log::{debug, error, info, warn};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, broadcast, mpsc};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use wmi::{COMLibrary, WMIConnection};

/// Notification sent when process list changes
#[derive(Clone, Debug)]
pub struct ProcessChangeNotification;

/// Intrinsic process-change subscription. `WITHIN 2` (not 1) halves how often
/// WmiPrvSE diffs the full process list - the top always-on cost since the
/// v3.0.3 audit - for +1s worst-case detection latency, which the reconcile net
/// bounds regardless. (An ETW-backed Win32_ProcessTrace path would avoid the
/// diff entirely but needs elevation; non-elevated it would fall back to exactly
/// this query, so this covers the common case.)
const PROCESS_EVENT_QUERY: &str = "SELECT * FROM __InstanceOperationEvent WITHIN 2 \
     WHERE TargetInstance ISA 'Win32_Process'";

/// Process operation event from WMI (covers both creation and deletion)
/// Uses __InstanceOperationEvent as the base query to receive both event types
/// on a single thread, halving COM/WMI overhead.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct ProcessOperationEvent {
    /// WMI system property: "__InstanceCreationEvent" or "__InstanceDeletionEvent"
    /// Optional for safety - if the wmi crate can't read __CLASS, events are
    /// skipped and the 60-second reconciliation catches them instead.
    #[serde(rename = "__CLASS", default)]
    class: Option<String>,
    target_instance: Win32Process,
}

/// Win32_Process instance
#[derive(Deserialize, Debug)]
#[serde(rename = "Win32_Process")]
#[serde(rename_all = "PascalCase")]
struct Win32Process {
    name: Option<String>,
    process_id: u32,
}

/// Shared process state - always up-to-date via WMI events
#[derive(Debug)]
pub struct ProcessState {
    /// Process names (original case, with .exe suffix)
    names: HashSet<Arc<str>>,
    /// Process ID to name mapping (for deletion lookup)
    pid_to_name: std::collections::HashMap<u32, Arc<str>>,
    /// Reference count per process name for O(1) removal
    name_counts: std::collections::HashMap<Arc<str>, u32>,
    /// Count of running .scr (screensaver) processes for O(1) lookup
    scr_count: u32,
    /// Last update time (for diagnostics)
    last_updated: Instant,
    /// Whether a successful enumeration has EVER completed.
    ///
    /// An empty `names` is ambiguous: it means either "no processes matched" or
    /// "we have never managed to look". Consumers that publish a sensor must not
    /// collapse the second into a confident negative, which is the same honesty
    /// rule the Linux twin follows via `try_resolve_processes`.
    enumerated: bool,
}

impl ProcessState {
    fn new() -> Self {
        Self {
            names: HashSet::new(),
            pid_to_name: std::collections::HashMap::new(),
            name_counts: std::collections::HashMap::new(),
            scr_count: 0,
            last_updated: Instant::now(),
            enumerated: false,
        }
    }

    /// Mark that a full enumeration has completed, however many processes it saw.
    fn mark_enumerated(&mut self) {
        self.enumerated = true;
    }

    fn add_process(&mut self, name: String, pid: u32) {
        // Idempotent per PID: a delayed WMI __InstanceCreationEvent can re-add a
        // PID that reconcile() already tracked. Without this, name_counts would
        // double-increment (only decremented once on removal), leaving a stale
        // process reported "running" forever + a slow leak.
        let existing_differs = match self.pid_to_name.get(&pid) {
            Some(existing) if existing.as_ref() == name.as_str() => return,
            Some(_) => true,
            None => false,
        };
        if existing_differs {
            self.remove_process(pid);
        }

        if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".scr") {
            self.scr_count += 1;
        }
        // Reuse existing Arc when the process name is already tracked (e.g. svchost.exe),
        // avoiding a fresh heap allocation per duplicate name.
        let arc_name: Arc<str> = if let Some(existing) = self.names.get(name.as_str()) {
            Arc::clone(existing)
        } else {
            Arc::from(name)
        };
        self.pid_to_name.insert(pid, Arc::clone(&arc_name));
        *self.name_counts.entry(Arc::clone(&arc_name)).or_insert(0) += 1;
        self.names.insert(arc_name);
        self.last_updated = Instant::now();
    }

    fn remove_process(&mut self, pid: u32) {
        if let Some(name) = self.pid_to_name.remove(&pid) {
            if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".scr") {
                self.scr_count = self.scr_count.saturating_sub(1);
            }
            if let Some(count) = self.name_counts.get_mut(&name) {
                *count -= 1;
                if *count == 0 {
                    self.name_counts.remove(&name);
                    self.names.remove(&name);
                }
            }
            self.last_updated = Instant::now();
        }
    }

    /// Get process names without cloning the set
    /// True once any enumeration has succeeded. See [`ProcessState::enumerated`].
    pub fn has_enumerated(&self) -> bool {
        self.enumerated
    }

    pub fn names(&self) -> &HashSet<Arc<str>> {
        &self.names
    }
}

/// Event sent from WMI threads to the async event processor
enum ProcessEvent {
    Created(String, u32), // name, pid
    Deleted(u32),         // pid
}

/// Event-driven process watcher
pub struct ProcessWatcher {
    /// Shared process state
    state: Arc<RwLock<ProcessState>>,
    /// Channel for notifying subscribers of process changes
    change_tx: broadcast::Sender<ProcessChangeNotification>,
}

impl ProcessWatcher {
    /// Create a new process watcher with initial enumeration
    pub async fn new() -> Self {
        let state = Arc::new(RwLock::new(ProcessState::new()));
        // Subscribers just need to know "something changed" - lag is handled gracefully
        let (change_tx, _) = broadcast::channel(16);

        // Initial enumeration using ToolHelp (fast, reliable)
        Self::initial_enumeration(&state).await;

        Self { state, change_tx }
    }

    /// Subscribe to process change notifications
    pub fn subscribe(&self) -> broadcast::Receiver<ProcessChangeNotification> {
        self.change_tx.subscribe()
    }

    /// Start the background WMI event watcher
    ///
    /// This should be called once after creating the ProcessWatcher.
    /// Spawns background threads for WMI event subscription.
    /// Falls back to polling if WMI fails.
    pub fn start_background(&self, shutdown_rx: broadcast::Receiver<()>, poll_interval: Duration) {
        let state = Arc::clone(&self.state);
        let change_tx = self.change_tx.clone();
        // WMI fires events for ALL processes. The WMI thread uses blocking_send(),
        // so a full channel stalls it rather than dropping events. WMI's WITHIN 2
        // batching means events arrive in ~2s bursts - 64 slots is ample.
        let (event_tx, event_rx) = mpsc::channel::<ProcessEvent>(64);

        // Try WMI events first, fall back to polling
        tokio::spawn(async move {
            match Self::setup_wmi_events(event_tx).await {
                Ok(()) => {
                    info!("Process watcher using WMI events");
                    // Close the startup blind window: a process started between the
                    // initial ToolHelp enumeration (in new()) and the WMI
                    // subscription going live is otherwise invisible until the first
                    // 60s reconcile. Re-snapshot now; add_process is idempotent per
                    // PID, so re-adding already-tracked processes is safe.
                    let (pruned, added) = Self::reconcile(&state).await;
                    if pruned > 0 || added > 0 {
                        let _ = change_tx.send(ProcessChangeNotification);
                    }
                    Self::run_event_processor(&state, shutdown_rx, change_tx, event_rx).await;
                }
                Err(e) => {
                    warn!(
                        "WMI event subscription failed, using polling fallback: {}",
                        e
                    );
                    Self::run_polling_fallback(&state, shutdown_rx, poll_interval, change_tx).await;
                }
            }
        });
    }

    /// Perform initial process enumeration using ToolHelp API.
    /// Reuses `snapshot_all_processes` to avoid code duplication.
    async fn initial_enumeration(state: &Arc<RwLock<ProcessState>>) {
        let processes = match tokio::task::spawn_blocking(Self::snapshot_all_processes).await {
            Ok(Some(s)) => s,
            // Do NOT mark_enumerated here: a failed enumeration must stay
            // indistinguishable from "not yet enumerated", or the game sensor
            // publishes "none" on the strength of a snapshot that never happened.
            Ok(None) => {
                error!("Initial process enumeration returned no snapshot");
                return;
            }
            Err(e) => {
                error!("Initial process enumeration failed: {}", e);
                return;
            }
        };

        let mut guard = state.write().await;
        for (pid, name) in processes {
            guard.add_process(name, pid);
        }
        guard.mark_enumerated();

        info!(
            "Initial process enumeration: {} processes",
            guard.names.len()
        );
    }

    /// Set up WMI event subscription for process changes
    ///
    /// Uses a single thread with `__InstanceOperationEvent` to capture both
    /// creation and deletion events, halving COM/WMI memory overhead.
    /// The `__CLASS` system property distinguishes event types.
    /// Falls back to polling if WMI setup fails.
    async fn setup_wmi_events(event_tx: mpsc::Sender<ProcessEvent>) -> anyhow::Result<()> {
        // Use a oneshot channel so the WMI thread can report whether
        // raw_notification() succeeded before we commit to event mode.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        // Single thread handles both creation and deletion events.
        // Shutdown: when the mpsc receiver is dropped, blocking_send() returns Err
        // and the thread breaks out of the event loop. The WMI `WITHIN 2` poll interval
        // means at most ~2s delay before the thread notices and exits.
        // JoinHandle is intentionally detached - the thread is cleaned up on process exit.
        std::thread::Builder::new()
            .name("wmi-events".into())
            // 256 KB: this thread runs COM marshaling + serde deserialization of
            // WMI objects, deeper call chains than the message-pump threads.
            .stack_size(256 * 1024)
            .spawn(move || {
                let com = match COMLibrary::new() {
                    Ok(c) => c,
                    Err(e) => {
                        let msg = format!("Failed to init COM for process events thread: {}", e);
                        error!("{}", msg);
                        let _ = ready_tx.send(Err(msg));
                        return;
                    }
                };
                let wmi = match WMIConnection::new(com) {
                    Ok(w) => w,
                    Err(e) => {
                        let msg = format!("Failed to create WMI connection: {}", e);
                        error!("{}", msg);
                        let _ = ready_tx.send(Err(msg));
                        return;
                    }
                };

                debug!("WMI process event thread started (creation + deletion)");

                // __InstanceOperationEvent is the parent of both __InstanceCreationEvent
                // and __InstanceDeletionEvent, so one query captures both types.
                let events =
                    match wmi.raw_notification::<ProcessOperationEvent>(PROCESS_EVENT_QUERY) {
                        Ok(iter) => {
                            info!("WMI process event subscription established");
                            let _ = ready_tx.send(Ok(()));
                            iter
                        }
                        Err(e) => {
                            let msg = format!("Failed to subscribe to process events: {}", e);
                            error!("{}", msg);
                            let _ = ready_tx.send(Err(msg));
                            return;
                        }
                    };

                let mut consecutive_errors = 0u32;
                for event_result in events {
                    match event_result {
                        Ok(event) => {
                            consecutive_errors = 0;
                            let pid = event.target_instance.process_id;
                            match event.class.as_deref() {
                                Some(c) if c.contains("Creation") => {
                                    if let Some(name) = event.target_instance.name
                                        && event_tx
                                            .blocking_send(ProcessEvent::Created(name, pid))
                                            .is_err()
                                    {
                                        break;
                                    }
                                }
                                Some(c) if c.contains("Deletion") => {
                                    if event_tx.blocking_send(ProcessEvent::Deleted(pid)).is_err() {
                                        break;
                                    }
                                }
                                _ => {
                                    // Unknown or missing __CLASS (e.g. modification events)
                                    // Skip - 60s reconciliation catches anything missed
                                    debug!("WMI: skipping event with class {:?}", event.class);
                                }
                            }
                        }
                        Err(e) => {
                            // Tolerate transient per-event errors (a single bad
                            // deserialize/COM hiccup): skip the event rather than
                            // tear down the whole subscription and force a full
                            // resubscribe + snapshot reconcile. Bail only if errors
                            // pile up, which means the stream is actually dead.
                            consecutive_errors += 1;
                            if consecutive_errors >= 10 {
                                error!("WMI event stream failing repeatedly ({e}); resubscribing");
                                break;
                            }
                            warn!("WMI process event error (skipping): {e}");
                        }
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("Failed to spawn WMI events thread: {e}"))?;

        // Wait for the WMI thread to confirm subscription succeeded
        match ready_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => Err(anyhow::anyhow!(msg)),
            Err(_) => Err(anyhow::anyhow!(
                "WMI event thread exited before reporting status"
            )),
        }
    }

    /// Process WMI events and run periodic reconciliation
    async fn run_event_processor(
        state: &Arc<RwLock<ProcessState>>,
        mut shutdown_rx: broadcast::Receiver<()>,
        change_tx: broadcast::Sender<ProcessChangeNotification>,
        mut event_rx: mpsc::Receiver<ProcessEvent>,
    ) {
        // Adaptive reconciliation: start at 60s, extend to 5min when WMI is healthy,
        // shrink back to 60s if reconciliation detects drift (missed events).
        const MIN_RECONCILE_SECS: u64 = 60;
        const MAX_RECONCILE_SECS: u64 = 300;
        let mut reconcile_secs = MIN_RECONCILE_SECS;
        let mut consecutive_clean = 0u32;
        let mut next_reconcile = tokio::time::Instant::now() + Duration::from_secs(reconcile_secs);
        let mut wmi_alive = true;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Event processor shutting down");
                    break;
                }
                result = event_rx.recv(), if wmi_alive => {
                    match result {
                        Some(event) => {
                            // Batch: drain all pending events before acquiring write lock
                            let mut batch = Vec::new();
                            batch.push(event);
                            while let Ok(e) = event_rx.try_recv() {
                                batch.push(e);
                            }

                            let mut guard = state.write().await;
                            for ev in batch {
                                match ev {
                                    ProcessEvent::Created(name, pid) => {
                                        debug!("Process started: {} (PID {})", name, pid);
                                        guard.add_process(name, pid);
                                    }
                                    ProcessEvent::Deleted(pid) => {
                                        // PID-reuse guard: a late deletion event for
                                        // a PID that has since been reused would evict
                                        // the live new process. Before evicting, check
                                        // the PID is actually gone; skip if it still
                                        // resolves to the image name we track for it.
                                        if let Some(name) = guard.pid_to_name.get(&pid).cloned()
                                            && process_alive_with_name(pid, &name)
                                        {
                                            debug!("Skipping stale deletion for reused PID {}", pid);
                                            continue;
                                        }
                                        debug!("Process ended: PID {}", pid);
                                        guard.remove_process(pid);
                                    }
                                }
                            }
                            drop(guard);
                            let _ = change_tx.send(ProcessChangeNotification);
                        }
                        None => {
                            // WMI event channel closed - thread died due to error.
                            // Throttle first so a persistent fault (setup succeeds
                            // but the stream dies immediately) can't become a tight
                            // restart+snapshot+fan-out loop that pegs a core.
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            // Attempt to restart the WMI thread before falling back.
                            warn!("WMI event stream lost, attempting restart...");
                            let (new_tx, new_rx) = mpsc::channel::<ProcessEvent>(64);
                            match Self::setup_wmi_events(new_tx).await {
                                Ok(()) => {
                                    info!("WMI event thread restarted successfully");
                                    event_rx = new_rx;
                                    // Reconcile immediately to catch anything missed
                                    let (pruned, added) = Self::reconcile(state).await;
                                    if pruned > 0 || added > 0 {
                                        let _ = change_tx.send(ProcessChangeNotification);
                                    }
                                    reconcile_secs = MIN_RECONCILE_SECS;
                                    consecutive_clean = 0;
                                }
                                Err(e) => {
                                    // Restart failed - fall back to reconciliation-only mode
                                    warn!("WMI restart failed: {}, falling back to reconciliation every {}s", e, MIN_RECONCILE_SECS);
                                    wmi_alive = false;
                                    reconcile_secs = MIN_RECONCILE_SECS;
                                    consecutive_clean = 0;
                                }
                            }
                            next_reconcile = tokio::time::Instant::now() + Duration::from_secs(reconcile_secs);
                        }
                    }
                }
                () = tokio::time::sleep_until(next_reconcile) => {
                    let (pruned, added) = Self::reconcile(state).await;
                    if pruned > 0 || added > 0 {
                        debug!("Reconciliation: pruned {} stale, added {} new entries, resetting interval to {}s", pruned, added, MIN_RECONCILE_SECS);
                        // Drift detected - shrink interval back to minimum
                        reconcile_secs = MIN_RECONCILE_SECS;
                        consecutive_clean = 0;
                        let _ = change_tx.send(ProcessChangeNotification);
                    } else {
                        consecutive_clean += 1;
                        // After 3 consecutive clean reconciliations, double the interval
                        if consecutive_clean >= 3 && reconcile_secs < MAX_RECONCILE_SECS {
                            reconcile_secs = (reconcile_secs * 2).min(MAX_RECONCILE_SECS);
                            debug!("WMI healthy, extending reconciliation to {}s", reconcile_secs);
                        }
                    }
                    next_reconcile = tokio::time::Instant::now() + Duration::from_secs(reconcile_secs);
                }
            }
        }
    }

    /// Periodic reconciliation: full process snapshot to catch missed WMI events
    /// and prune stale PID entries (prevents memory leak from WMI event loss).
    /// Returns (pruned, added) counts for callers to decide whether to notify.
    async fn reconcile(state: &Arc<RwLock<ProcessState>>) -> (usize, usize) {
        let snapshot = match tokio::task::spawn_blocking(Self::snapshot_all_processes).await {
            Ok(Some(s)) => s,
            // Skip BOTH the mark and the prune. Pruning against a snapshot that
            // failed would expire every tracked PID at once.
            _ => return (0, 0),
        };

        let mut guard = state.write().await;
        guard.mark_enumerated();

        // Remove PIDs no longer running
        let expired: Vec<u32> = guard
            .pid_to_name
            .keys()
            .filter(|pid| !snapshot.contains_key(pid))
            .copied()
            .collect();

        let pruned = expired.len();
        for pid in expired {
            guard.remove_process(pid);
        }

        // Add PIDs running but not tracked
        let mut added = 0usize;
        for (pid, name) in snapshot {
            if !guard.pid_to_name.contains_key(&pid) {
                guard.add_process(name, pid);
                added += 1;
            }
        }

        // Reclaim excess capacity only when significantly oversized
        // Avoids churning the allocator on small fluctuations
        let cap = guard.pid_to_name.capacity();
        let len = guard.pid_to_name.len();
        if cap > 256 && cap > len * 3 {
            let target = len.next_power_of_two().max(64);
            guard.pid_to_name.shrink_to(target);
            guard.name_counts.shrink_to(target);
            guard.names.shrink_to(target);
        }

        (pruned, added)
    }

    /// Take a full process snapshot using ToolHelp API
    /// `None` means the enumeration FAILED, which is not the same as "no
    /// processes are running". `CreateToolhelp32Snapshot` fails transiently with
    /// `ERROR_BAD_LENGTH` under memory pressure and is documented as
    /// retry-worthy. Collapsing that into an empty map made callers mark the
    /// table enumerated with zero entries, prune every tracked PID, and publish
    /// a confident "no game running" while a game was running.
    fn snapshot_all_processes() -> Option<std::collections::HashMap<u32, String>> {
        let mut pids = std::collections::HashMap::new();

        // SAFETY: CreateToolhelp32Snapshot and Process32 enumeration are
        // standard Win32 APIs for process listing. Handle is properly closed.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                warn!("CreateToolhelp32Snapshot failed; treating as unobserved");
                return None;
            };
            {
                let mut entry = PROCESSENTRY32W {
                    dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };

                if Process32FirstW(snapshot, &raw mut entry).is_ok() {
                    loop {
                        // Slice to null terminator before converting, avoiding
                        // a 260-char allocation for every process entry.
                        let nul_pos = entry
                            .szExeFile
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szExeFile.len());
                        let name = String::from_utf16_lossy(&entry.szExeFile[..nul_pos]);
                        if !name.is_empty() {
                            pids.insert(entry.th32ProcessID, name);
                        }
                        if Process32NextW(snapshot, &raw mut entry).is_err() {
                            break;
                        }
                    }
                }

                let _ = CloseHandle(snapshot);
            }
        }

        Some(pids)
    }

    /// Polling fallback if WMI events aren't available.
    /// Uses reconcile (snapshot + diff) to both add new and remove stale processes.
    async fn run_polling_fallback(
        state: &Arc<RwLock<ProcessState>>,
        mut shutdown_rx: broadcast::Receiver<()>,
        poll_interval: Duration,
        change_tx: broadcast::Sender<ProcessChangeNotification>,
    ) {
        let mut interval = tokio::time::interval(poll_interval);
        // Skip missed ticks so a long reconcile (or resume from sleep) doesn't
        // fire a burst of catch-up polls, consistent with the other sensors.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let state = Arc::clone(state);

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Process watcher (polling) shutting down");
                    break;
                }
                _ = interval.tick() => {
                    let (pruned, added) = Self::reconcile(&state).await;
                    // Notify subscribers after each poll if anything changed
                    if pruned > 0 || added > 0 {
                        let _ = change_tx.send(ProcessChangeNotification);
                    }
                }
            }
        }
    }

    /// Get a snapshot of current process names
    pub async fn get_names(&self) -> HashSet<Arc<str>> {
        self.state.read().await.names.clone()
    }

    /// Check if any screensaver process is running (O(1) via tracked counter)
    pub async fn has_screensaver_running(&self) -> bool {
        self.state.read().await.scr_count > 0
    }

    /// Get the underlying shared state for direct access
    pub fn state(&self) -> Arc<RwLock<ProcessState>> {
        Arc::clone(&self.state)
    }

    /// Backend-independent live check for one specific process by name, via a
    /// ToolHelp snapshot. Needed for one-off checks like "is Steam running" that
    /// must work under the window backend too, whose ProcessState only tracks
    /// WATCHED games - not arbitrary processes like steam.exe. Runs the blocking
    /// snapshot off the runtime.
    pub async fn is_process_running(name: &str) -> bool {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            // A failed snapshot answers "not running", same as before this
            // returned Option. Callers use this as a gate (is Steam up?), where
            // failing closed is the safe direction.
            Self::snapshot_all_processes()
                .is_some_and(|p| p.values().any(|n| n.eq_ignore_ascii_case(&name)))
        })
        .await
        .unwrap_or(false)
    }
}

/// Whether `pid` currently refers to a live process whose image basename equals
/// `expected_name` (case-insensitive). Used to detect PID reuse before acting on
/// a deletion event: if the PID still resolves to the name we track, the old
/// process's deletion event is stale and evicting would drop the live one.
///
/// Returns false if the process can't be opened (gone, or access denied - in
/// which case we let the eviction proceed; the reconcile net corrects any error).
fn process_alive_with_name(pid: u32, expected_name: &str) -> bool {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    // SAFETY: OpenProcess returns a handle we close; QueryFullProcessImageNameW
    // writes into a fixed buffer whose length we pass and read back.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            windows::Win32::System::Threading::PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok {
            return false;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        // Compare basenames case-insensitively (stored name is like "chrome.exe").
        full.rsplit(['\\', '/'])
            .next()
            .is_some_and(|base| base.eq_ignore_ascii_case(expected_name))
    }
}

// ============================================================================
// Window-event detection backend (Windows) - config detection_backend = "window"
// ============================================================================
//
// Instead of WmiPrvSE diffing the whole process table every 2s, this backend
// learns a game started from its top-level window appearing (SetWinEventHook,
// non-admin, event-driven), resolves the window to its process, and admits it to
// ProcessState only if a consumer WATCHES it (a game / custom ProcessExists /
// screensaver) - so browsers and the shell never enter the set. Stop is also
// event-driven: a dedicated thread WaitForMultipleObjects on the admitted games'
// SYNCHRONIZE handles (plus a wake event), so a game closing signals instantly
// with no polling. Headless watched patterns (no window) are the one exception -
// they have no window to hook, so a targeted snapshot finds their start/stop, and
// that loop only runs while the headless set is non-empty.
//
// UNVERIFIED off Windows: this is type-checked via cargo xwin but the WinEvent /
// OpenProcess / message-pump behavior needs the live-box checklist in the plan
// before detection_backend defaults to "window".

#[cfg(windows)]
impl ProcessWatcher {
    /// Start the event-driven window-detection backend. `app` gives live config
    /// access (which processes are watched) and the config-generation signal.
    pub fn start_background_window(
        &self,
        shutdown_rx: broadcast::Receiver<()>,
        app: Arc<crate::AppState>,
    ) {
        let state = Arc::clone(&self.state);
        let change_tx = self.change_tx.clone();
        tokio::spawn(async move { run_window_backend(state, change_tx, shutdown_rx, app).await });
    }
}

/// A Windows HANDLE we own and will close, wrapped so it can cross the
/// spawn_blocking / channel boundary to the exit-watcher thread. Sound because
/// exactly one place ever holds and closes each handle (the exit thread).
#[cfg(windows)]
struct SendHandle(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for SendHandle {}

/// Command from the async loop to the exit-watcher thread.
#[cfg(windows)]
enum ExitCmd {
    /// Watch this PID's SYNCHRONIZE handle for exit.
    Watch(u32, SendHandle),
    /// Stop watching (game removed from config); closes the handle.
    Unwatch(u32),
    /// Tear down the thread and close all handles.
    Shutdown,
}

/// Handle to the exit-watcher thread. Sending a command also wakes the thread out
/// of WaitForMultipleObjects via the auto-reset wake event.
#[cfg(windows)]
struct ExitWatcher {
    cmd_tx: std::sync::mpsc::Sender<ExitCmd>,
    wake: isize, // wake event HANDLE as isize (Send)
}

#[cfg(windows)]
impl ExitWatcher {
    fn send(&self, cmd: ExitCmd) {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::SetEvent;
        if self.cmd_tx.send(cmd).is_err() {
            return;
        }
        // SAFETY: signaling our own auto-reset event to break the wait.
        unsafe {
            let _ = SetEvent(HANDLE(self.wake as *mut core::ffi::c_void));
        }
    }
    fn watch(&self, pid: u32, handle: SendHandle) {
        self.send(ExitCmd::Watch(pid, handle));
    }
    fn unwatch(&self, pid: u32) {
        self.send(ExitCmd::Unwatch(pid));
    }
    fn shutdown(&self) {
        self.send(ExitCmd::Shutdown);
    }
}

/// Spawn the dedicated exit-watcher thread. It WaitForMultipleObjects over the
/// wake event (index 0) plus each watched game's SYNCHRONIZE handle; a game handle
/// signaling means that PID exited, reported on `exited_tx`. One thread owns every
/// handle and closes it, so there is no cross-thread unregister/close race.
#[cfg(windows)]
fn spawn_exit_watcher(exited_tx: mpsc::Sender<u32>) -> Option<ExitWatcher> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};
    // Win32 MAXIMUM_WAIT_OBJECTS (wake event + up to 63 process handles).
    const MAXIMUM_WAIT_OBJECTS: usize = 64;

    // Auto-reset, initially unsignaled wake event.
    // SAFETY: standard event creation; handle is closed on thread exit.
    let wake = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            error!("Exit watcher: CreateEventW failed: {:?}", e);
            return None;
        }
    };
    let wake_isize = wake.0 as isize;
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ExitCmd>();

    let spawned = std::thread::Builder::new()
        .name("proc-exit".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            // Reconstruct the wake HANDLE inside the thread (HANDLE isn't Send, so
            // only its isize crossed the boundary). handles[0] is the wake event;
            // handles[1..] parallel pids[..].
            let mut handles: Vec<HANDLE> = vec![HANDLE(wake_isize as *mut core::ffi::c_void)];
            let mut pids: Vec<u32> = Vec::new();
            let mut stop = false;
            while !stop {
                // SAFETY: waiting on our own event + process handles we own.
                let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                if r == WAIT_FAILED {
                    error!("Exit watcher: WaitForMultipleObjects failed");
                    break;
                }
                let idx = (r.0 - WAIT_OBJECT_0.0) as usize;
                if idx == 0 {
                    // Wake: drain commands.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            ExitCmd::Watch(pid, h) => {
                                if handles.len() < MAXIMUM_WAIT_OBJECTS {
                                    handles.push(h.0);
                                    pids.push(pid);
                                } else {
                                    warn!(
                                        "Exit watcher full ({} handles); not watching PID {}",
                                        handles.len(),
                                        pid
                                    );
                                    // SAFETY: dropping a handle we won't track.
                                    unsafe {
                                        let _ = CloseHandle(h.0);
                                    }
                                }
                            }
                            ExitCmd::Unwatch(pid) => {
                                if let Some(i) = pids.iter().position(|p| *p == pid) {
                                    // SAFETY: closing the handle we opened for this pid.
                                    unsafe {
                                        let _ = CloseHandle(handles[i + 1]);
                                    }
                                    handles.remove(i + 1);
                                    pids.remove(i);
                                }
                            }
                            ExitCmd::Shutdown => stop = true,
                        }
                    }
                } else if idx <= pids.len() {
                    // A watched process exited.
                    let pid = pids[idx - 1];
                    // SAFETY: closing the exited process's handle.
                    unsafe {
                        let _ = CloseHandle(handles[idx]);
                    }
                    handles.remove(idx);
                    pids.remove(idx - 1);
                    let _ = exited_tx.try_send(pid);
                }
            }
            // Close every remaining handle, including the wake event at index 0.
            for h in &handles {
                // SAFETY: final cleanup of handles this thread exclusively owns.
                unsafe {
                    let _ = CloseHandle(*h);
                }
            }
        });

    if let Err(e) = spawned {
        error!("Failed to spawn exit-watcher thread: {}", e);
        // SAFETY: close the orphaned wake event.
        unsafe {
            let _ = CloseHandle(wake);
        }
        return None;
    }
    Some(ExitWatcher {
        cmd_tx,
        wake: wake_isize,
    })
}

#[cfg(windows)]
async fn run_window_backend(
    state: Arc<RwLock<ProcessState>>,
    change_tx: broadcast::Sender<ProcessChangeNotification>,
    mut shutdown_rx: broadcast::Receiver<()>,
    app: Arc<crate::AppState>,
) {
    // Top-level window HWNDs from the hook thread (as isize; HWND isn't Send).
    let (hwnd_tx, mut hwnd_rx) = mpsc::channel::<isize>(64);
    let hook_waiter = spawn_window_event_thread(hwnd_tx, app.shutdown_tx.subscribe());

    // Exited-PID reports from the WaitForMultipleObjects thread.
    let (exited_tx, mut exited_rx) = mpsc::channel::<u32>(64);
    let exit = spawn_exit_watcher(exited_tx);

    // Admit games already running before we started (window events won't refire).
    window_reconcile_watched(&state, &change_tx, &app, exit.as_ref()).await;

    let mut config_rx = app.config_generation.subscribe();
    // Headless poll: only meaningful when detect:"poll" games / ProcessExists
    // sensors exist. When they don't, the branch is guarded off so nothing polls.
    let mut headless_active = !app.config.read().await.headless_watch_names().is_empty();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // consume the immediate first tick

    info!("Process watcher using window events");
    // Set when the hook thread dies (window creation or hook registration
    // failed, or it exited unexpectedly). Without this the channel simply
    // closes and detection goes blind for the whole process lifetime behind a
    // single error line - the WMI backend has always had a polling fallback,
    // and since `window` is now the default this one needs the same safety net.
    let mut hook_failed = false;
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => break,
            maybe = hwnd_rx.recv() => {
                match maybe {
                    // None means the hook thread exited (window creation or hook
                    // registration failed), dropping its sender. NOTE: the `0`
                    // sentinel goes on the separate oneshot used for the HWND
                    // handoff, not this mpsc, and win_event_proc filters invalid
                    // HWNDs - so `None` is the only signal available here.
                    None => {
                        hook_failed = true;
                        break;
                    }
                    Some(hwnd) => {
                        window_admit_hwnd(hwnd, &state, &change_tx, &app, exit.as_ref()).await;
                    }
                }
            }
            Some(pid) = exited_rx.recv() => {
                // A watched game's window process exited - drop it.
                if state.read().await.pid_to_name.contains_key(&pid) {
                    state.write().await.remove_process(pid);
                    let _ = change_tx.send(ProcessChangeNotification);
                }
            }
            r = config_rx.recv() => {
                if matches!(r, Ok(()) | Err(broadcast::error::RecvError::Lagged(_))) {
                    window_reconcile_watched(&state, &change_tx, &app, exit.as_ref()).await;
                    headless_active = !app.config.read().await.headless_watch_names().is_empty();
                }
            }
            _ = tick.tick(), if headless_active => {
                window_headless_poll(&state, &change_tx, &app).await;
            }
        }
    }
    if let Some(e) = &exit {
        e.shutdown();
    }
    if let Some(h) = hook_waiter {
        if hook_failed {
            // The waiter parks on the GLOBAL shutdown channel, so awaiting it
            // here would block until the agent exits and the polling fallback
            // below would never start - leaving detection blind, which is the
            // exact failure this fallback exists to prevent. Abort instead.
            h.abort();
        } else {
            let _ = h.await;
        }
    }

    // Degrade rather than go blind. Polling costs more than window events but
    // it keeps game/screensaver/idle detection alive, and the supervisor cannot
    // help here because this task is still "running".
    if hook_failed {
        let poll_interval =
            Duration::from_secs(app.config.read().await.intervals.game_sensor.max(5));
        warn!(
            "Window-event backend unavailable; falling back to polling every {}s",
            poll_interval.as_secs()
        );
        ProcessWatcher::run_polling_fallback(&state, shutdown_rx, poll_interval, change_tx).await;
    }
}

/// Resolve a single HWND to (pid, image basename, exit handle) and admit it if
/// watched and new, registering its SYNCHRONIZE handle with the exit watcher.
#[cfg(windows)]
async fn window_admit_hwnd(
    hwnd: isize,
    state: &Arc<RwLock<ProcessState>>,
    change_tx: &broadcast::Sender<ProcessChangeNotification>,
    app: &Arc<crate::AppState>,
    exit: Option<&ExitWatcher>,
) {
    // Filter + resolve off the runtime (OpenProcess/Query can block briefly). The
    // returned handle (if any) is a SYNCHRONIZE handle for exit-watching.
    let resolved = tokio::task::spawn_blocking(move || {
        if is_top_level_app_window(hwnd) {
            resolve_window_process(hwnd)
        } else {
            None
        }
    })
    .await
    .ok()
    .flatten();
    let Some((pid, name, handle)) = resolved else {
        return;
    };
    // Not new, or not watched: close the exit handle and stop.
    let already = state.read().await.pid_to_name.contains_key(&pid);
    let watched = !already && app.config.read().await.is_watched_process(&name);
    if !watched {
        if let Some(h) = handle {
            // SAFETY: closing a handle we opened but won't track.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(h.0);
            }
        }
        return;
    }
    debug!("Window backend: admitting {} (PID {})", name, pid);
    state.write().await.add_process(name, pid);
    let _ = change_tx.send(ProcessChangeNotification);
    // Register for event-driven exit, or drop the handle if we have no watcher.
    match (exit, handle) {
        (Some(w), Some(h)) => w.watch(pid, h),
        (_, Some(h)) => {
            // SAFETY: no watcher (spawn failed); close the unused handle.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(h.0);
            }
        }
        (_, None) => {
            // SYNCHRONIZE was denied (rare elevated process): admitted, but its exit
            // won't be caught until a config-change reconcile or restart.
            debug!("No exit handle for PID {} (SYNCHRONIZE denied)", pid);
        }
    }
}

/// Reconcile the headless watched patterns (detect:"poll" games / ProcessExists):
/// add those running but untracked, remove those tracked-by-headless-name that
/// vanished. Windowed games are untouched here (the exit watcher handles them).
#[cfg(windows)]
async fn window_headless_poll(
    state: &Arc<RwLock<ProcessState>>,
    change_tx: &broadcast::Sender<ProcessChangeNotification>,
    app: &Arc<crate::AppState>,
) {
    let headless = app.config.read().await.headless_watch_names();
    if headless.is_empty() {
        return;
    }
    // MUST match Config::headless_watch_names, which produces the list this is
    // compared against by exact string equality. The old form only handled
    // all-lower and all-UPPER extensions, so a process named "Game.Exe"
    // normalized to "game.exe" here but to "game" there, and a headless game
    // with that casing was never detected or never cleared.
    let stem = |name: &str| -> String { crate::commands::strip_exe(name).to_lowercase() };
    // A failed snapshot must not be read as "the headless game exited".
    let Ok(Some(snapshot)) =
        tokio::task::spawn_blocking(ProcessWatcher::snapshot_all_processes).await
    else {
        return;
    };
    let mut changed = false;
    let mut guard = state.write().await;
    // Add headless matches not tracked.
    for (pid, name) in &snapshot {
        if guard.pid_to_name.contains_key(pid) {
            continue;
        }
        if headless.iter().any(|h| h == &stem(name)) {
            guard.add_process(name.clone(), *pid);
            changed = true;
        }
    }
    // Remove tracked headless-named PIDs that vanished (windowed games excluded:
    // their name won't be in the headless set).
    let gone: Vec<u32> = guard
        .pid_to_name
        .iter()
        .filter(|(pid, name)| {
            headless.iter().any(|h| h == &stem(name)) && !snapshot.contains_key(pid)
        })
        .map(|(pid, _)| *pid)
        .collect();
    for pid in gone {
        guard.remove_process(pid);
        changed = true;
    }
    drop(guard);
    if changed {
        let _ = change_tx.send(ProcessChangeNotification);
    }
}

/// Full re-evaluation: admit already-running watched top-level windows and drop
/// tracked entries no longer watched (after startup and on config change).
#[cfg(windows)]
async fn window_reconcile_watched(
    state: &Arc<RwLock<ProcessState>>,
    change_tx: &broadcast::Sender<ProcessChangeNotification>,
    app: &Arc<crate::AppState>,
    exit: Option<&ExitWatcher>,
) {
    // Drop tracked entries that are no longer watched (a game removed from config).
    {
        let cfg = app.config.read().await;
        let stale: Vec<u32> = {
            let guard = state.read().await;
            guard
                .pid_to_name
                .iter()
                .filter(|(_, name)| !cfg.is_watched_process(name))
                .map(|(pid, _)| *pid)
                .collect()
        };
        if !stale.is_empty() {
            let mut guard = state.write().await;
            for pid in &stale {
                guard.remove_process(*pid);
            }
            drop(guard);
            if let Some(w) = exit {
                for pid in &stale {
                    w.unwatch(*pid); // stop watching + close the handle
                }
            }
            let _ = change_tx.send(ProcessChangeNotification);
        }
    }
    // Admit currently-open watched top-level windows.
    let hwnds = tokio::task::spawn_blocking(enum_top_level_windows)
        .await
        .unwrap_or_default();
    for hwnd in hwnds {
        window_admit_hwnd(hwnd, state, change_tx, app, exit).await;
    }
}

/// Whether `hwnd` is a real, visible, top-level application window (not a child,
/// owned dialog, or tool window like a menu/tooltip that OBJECT_SHOW also fires on).
#[cfg(windows)]
fn is_top_level_app_window(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GA_ROOT, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetWindow, GetWindowLongW, IsWindowVisible,
        WS_EX_TOOLWINDOW,
    };
    let h = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: all read-only window queries on a HWND the hook handed us.
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return false;
        }
        if GetAncestor(h, GA_ROOT) != h {
            return false; // a child window
        }
        if let Ok(owner) = GetWindow(h, GW_OWNER)
            && !owner.is_invalid()
        {
            return false; // an owned window (dialog)
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false; // tool window (menu/tooltip/IME)
        }
    }
    true
}

/// Resolve a top-level window to (pid, image basename, exit handle). Opens with
/// SYNCHRONIZE so the returned handle can be watched for exit; falls back to
/// limited-info-only (handle = None) if SYNCHRONIZE is denied (rare elevated
/// process) so the name still resolves. The limited-info right is why elevated /
/// anti-cheat game processes resolve at all. The caller owns and closes any
/// returned handle.
#[cfg(windows)]
fn resolve_window_process(hwnd: isize) -> Option<(u32, String, Option<SendHandle>)> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    use windows::core::PWSTR;

    let h = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: GetWindowThreadProcessId writes pid; the process handle is either
    // returned to the caller or closed here; QueryFullProcessImageNameW writes into
    // a fixed buffer whose length we pass and read back.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&raw mut pid));
        if pid == 0 {
            return None;
        }
        // Prefer a SYNCHRONIZE handle (for exit-watching); fall back to query-only.
        let (handle, can_wait) = match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            Ok(hproc) => (hproc, true),
            Err(_) => (
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?,
                false,
            ),
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
        .is_ok();
        if !ok {
            let _ = CloseHandle(handle);
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let base = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
        if base.is_empty() {
            let _ = CloseHandle(handle);
            return None;
        }
        if can_wait {
            Some((pid, base, Some(SendHandle(handle))))
        } else {
            let _ = CloseHandle(handle);
            Some((pid, base, None))
        }
    }
}

/// Enumerate all top-level windows' HWNDs (as isize) for a startup/rescan snapshot.
#[cfg(windows)]
fn enum_top_level_windows() -> Vec<isize> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the &mut Vec<isize> pointer we passed to EnumWindows.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        out.push(hwnd.0 as isize);
        TRUE
    }

    let mut out: Vec<isize> = Vec::new();
    // SAFETY: EnumWindows drives cb synchronously; the &mut Vec outlives the call.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&raw mut out as isize));
    }
    out
}

/// Spawn the SetWinEventHook message-pump thread. Hooks FOREGROUND + OBJECT_SHOW
/// and forwards each event's HWND (isize) to `event_tx`. Mirrors the ActiveWindow
/// focus monitor: OUTOFCONTEXT hooks, WM_USER to unblock GetMessageW at shutdown,
/// both hooks unhooked and the window destroyed on exit.
#[cfg(windows)]
fn spawn_window_event_thread(
    event_tx: mpsc::Sender<isize>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
        RegisterClassExW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_USER, WNDCLASSEXW,
    };

    // EVENT_SYSTEM_FOREGROUND = 0x3, EVENT_OBJECT_SHOW = 0x8002,
    // OBJID_WINDOW = 0, CHILDID_SELF = 0, WINEVENT_OUTOFCONTEXT = 0x2.
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
    const EVENT_OBJECT_SHOW: u32 = 0x8002;
    const WINEVENT_OUTOFCONTEXT: u32 = 0x0002;
    const OBJID_WINDOW: i32 = 0;
    const CHILDID_SELF: i32 = 0;

    thread_local! {
        static WIN_TX: std::cell::RefCell<Option<mpsc::Sender<isize>>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn win_event_proc(
        _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
        _event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _thread: u32,
        _time: u32,
    ) {
        // Only real top-level window events; skip child/non-window objects here so
        // the async side isn't flooded (it re-checks with is_top_level_app_window).
        if hwnd.is_invalid() || id_object != OBJID_WINDOW || id_child != CHILDID_SELF {
            return;
        }
        let raw = hwnd.0 as isize;
        WIN_TX.with(|tx| {
            if let Some(sender) = tx.borrow().as_ref() {
                let _ = sender.try_send(raw); // drop on backpressure; rescan catches it
            }
        });
    }

    let (hwnd_tx, hwnd_rx) = tokio::sync::oneshot::channel::<isize>();

    let spawned = std::thread::Builder::new()
        .name("window-events".into())
        .stack_size(256 * 1024)
        .spawn(move || unsafe {
            unsafe extern "system" fn wnd_proc(
                hwnd: HWND,
                msg: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> windows::Win32::Foundation::LRESULT {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }

            WIN_TX.with(|tx| *tx.borrow_mut() = Some(event_tx));

            let class_name = windows::core::w!("PCAgentWindowMonitor");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassExW(&raw const wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Window Monitor"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    error!("Failed to create window monitor window: {:?}", e);
                    let _ = hwnd_tx.send(0);
                    return;
                }
            };

            let hook_fg = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            let hook_show = SetWinEventHook(
                EVENT_OBJECT_SHOW,
                EVENT_OBJECT_SHOW,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            if hook_fg.is_invalid() || hook_show.is_invalid() {
                // RETURN, do not fall through into GetMessageW. Staying alive
                // holds the mpsc sender open, so the backend's `hwnd_rx.recv()`
                // never yields None, `hook_failed` never sets, and the polling
                // fallback never runs - leaving detection blind for the process
                // lifetime behind this one error line. Hook registration is also
                // the MORE likely failure (session/desktop isolation, UIPI) than
                // CreateWindowExW, so this is the case the fallback most needs.
                error!("Failed to set window-event hooks; falling back to polling");
                if !hook_fg.is_invalid() {
                    let _ = UnhookWinEvent(hook_fg);
                }
                if !hook_show.is_invalid() {
                    let _ = UnhookWinEvent(hook_show);
                }
                let _ = DestroyWindow(hwnd);
                // Drop the sender EXPLICITLY. What arms the polling fallback is
                // `hwnd_rx.recv()` yielding None, which needs this closed; relying
                // on the thread_local destructor at thread exit would silently stop
                // working if WIN_TX ever became a static/OnceLock, and the failure
                // mode is detection going blind - the thing the fallback exists for.
                WIN_TX.with(|tx| *tx.borrow_mut() = None);
                return;
            }

            let _ = hwnd_tx.send(hwnd.0 as isize);

            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&raw mut msg, None, 0, 0);
                if !ret.as_bool() || ret.0 == -1 {
                    break;
                }
                if msg.message == WM_USER {
                    break;
                }
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }

            if !hook_fg.is_invalid() {
                let _ = UnhookWinEvent(hook_fg);
            }
            if !hook_show.is_invalid() {
                let _ = UnhookWinEvent(hook_show);
            }
            let _ = DestroyWindow(hwnd);
        });

    if let Err(e) = spawned {
        error!("Failed to spawn window-events thread: {}", e);
        return None;
    }

    // Shutdown waiter: posts WM_USER to unblock GetMessageW so the thread exits.
    Some(tokio::spawn(async move {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};
        let hwnd_val = hwnd_rx.await.unwrap_or(0);
        let _ = shutdown_rx.recv().await;
        if hwnd_val != 0 {
            let h = HWND(hwnd_val as *mut core::ffi::c_void);
            // SAFETY: posting WM_USER to our own monitor window to break its pump.
            unsafe {
                let _ = PostMessageW(h, WM_USER, WPARAM(0), LPARAM(0));
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_event_query_polls_every_two_seconds() {
        // Guard against a regression back to WITHIN 1 (double the always-on cost).
        assert!(PROCESS_EVENT_QUERY.contains("WITHIN 2"));
        assert!(!PROCESS_EVENT_QUERY.contains("WITHIN 1"));
        assert!(PROCESS_EVENT_QUERY.contains("Win32_Process"));
    }

    #[test]
    fn test_process_state_add_remove() {
        let mut state = ProcessState::new();

        // Add processes
        state.add_process("chrome.exe".to_string(), 1234);
        state.add_process("firefox.exe".to_string(), 5678);

        assert!(state.names.contains("chrome.exe"));
        assert!(state.names.contains("firefox.exe"));
        assert_eq!(state.pid_to_name.len(), 2);

        // Remove a process
        state.remove_process(1234);
        assert!(!state.names.contains("chrome.exe"));
        assert!(state.names.contains("firefox.exe"));
        assert_eq!(state.pid_to_name.len(), 1);
    }

    #[test]
    fn test_process_state_duplicate_name() {
        let mut state = ProcessState::new();

        // Two processes with same name (e.g., multiple Chrome instances)
        state.add_process("chrome.exe".to_string(), 1000);
        state.add_process("chrome.exe".to_string(), 2000);

        assert!(state.names.contains("chrome.exe"));
        assert_eq!(state.pid_to_name.len(), 2);

        // Remove one - name should still exist
        state.remove_process(1000);
        assert!(state.names.contains("chrome.exe")); // Still running as PID 2000
        assert_eq!(state.pid_to_name.len(), 1);

        // Remove the other
        state.remove_process(2000);
        assert!(!state.names.contains("chrome.exe"));
        assert_eq!(state.pid_to_name.len(), 0);
    }

    #[test]
    fn test_process_state_remove_unknown() {
        let mut state = ProcessState::new();
        state.add_process("test.exe".to_string(), 100);

        // Removing unknown PID should not panic
        state.remove_process(999);

        assert!(state.names.contains("test.exe"));
    }

    #[test]
    fn test_screensaver_count() {
        let mut state = ProcessState::new();
        assert_eq!(state.scr_count, 0);

        state.add_process("Mystify.scr".to_string(), 100);
        assert_eq!(state.scr_count, 1);

        // Case-insensitive
        state.add_process("Bubbles.SCR".to_string(), 101);
        assert_eq!(state.scr_count, 2);

        state.remove_process(100);
        assert_eq!(state.scr_count, 1);

        state.remove_process(101);
        assert_eq!(state.scr_count, 0);

        // Non-.scr files don't affect count
        state.add_process("chrome.exe".to_string(), 200);
        assert_eq!(state.scr_count, 0);
    }
}
