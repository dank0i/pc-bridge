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

/// Process operation event from WMI (covers both creation and deletion)
/// Uses __InstanceOperationEvent as the base query to receive both event types
/// on a single thread, halving COM/WMI overhead.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct ProcessOperationEvent {
    /// WMI system property: "__InstanceCreationEvent" or "__InstanceDeletionEvent"
    /// Optional for safety — if the wmi crate can't read __CLASS, events are
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
}

impl ProcessState {
    fn new() -> Self {
        Self {
            names: HashSet::with_capacity(128),
            pid_to_name: std::collections::HashMap::with_capacity(128),
            name_counts: std::collections::HashMap::with_capacity(128),
            scr_count: 0,
            last_updated: Instant::now(),
        }
    }

    fn add_process(&mut self, name: String, pid: u32) {
        if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".scr") {
            self.scr_count += 1;
        }
        // Single Arc allocation shared across all three data structures
        let arc_name: Arc<str> = Arc::from(name);
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
        // Channel capacity of 256 is generous - subscribers just need to know "something changed"
        let (change_tx, _) = broadcast::channel(256);

        // Initial enumeration using ToolHelp (fast, reliable)
        Self::initial_enumeration(&state).await;

        Self { state, change_tx }
    }

    /// Subscribe to process change notifications
    pub fn subscribe(&self) -> broadcast::Receiver<ProcessChangeNotification> {
        self.change_tx.subscribe()
    }

    /// Notify subscribers that processes changed
    #[allow(dead_code)]
    fn notify_change(&self) {
        // Ignore send errors - means no subscribers
        let _ = self.change_tx.send(ProcessChangeNotification);
    }

    /// Start the background WMI event watcher
    ///
    /// This should be called once after creating the ProcessWatcher.
    /// Spawns background threads for WMI event subscription.
    /// Falls back to polling if WMI fails.
    pub fn start_background(&self, shutdown_rx: broadcast::Receiver<()>, poll_interval: Duration) {
        let state = Arc::clone(&self.state);
        let change_tx = self.change_tx.clone();
        // WMI fires events for ALL processes system-wide. Burst scenarios (compilers,
        // installers) can spawn dozens of processes in rapid succession. The WMI thread
        // uses blocking_send(), so a full channel stalls it rather than dropping events.
        // 256 slots avoids stalling the WMI thread during heavy bursts.
        let (event_tx, event_rx) = mpsc::channel::<ProcessEvent>(256);

        // Try WMI events first, fall back to polling
        tokio::spawn(async move {
            match Self::setup_wmi_events(event_tx).await {
                Ok(()) => {
                    info!("Process watcher using WMI events");
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
    /// Win32 snapshot runs on a blocking thread to avoid stalling the async runtime.
    async fn initial_enumeration(state: &Arc<RwLock<ProcessState>>) {
        let processes = tokio::task::spawn_blocking(|| -> Vec<(String, u32)> {
            let mut results = Vec::with_capacity(256);
            unsafe {
                if let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                    let mut entry = PROCESSENTRY32W {
                        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                        ..Default::default()
                    };

                    if Process32FirstW(snapshot, &raw mut entry).is_ok() {
                        loop {
                            let mut name = String::from_utf16_lossy(&entry.szExeFile);
                            if let Some(pos) = name.find('\0') {
                                name.truncate(pos);
                            }

                            if !name.is_empty() {
                                results.push((name, entry.th32ProcessID));
                            }

                            if Process32NextW(snapshot, &raw mut entry).is_err() {
                                break;
                            }
                        }
                    }

                    let _ = CloseHandle(snapshot);
                }
            }
            results
        })
        .await
        .unwrap_or_default();

        let mut guard = state.write().await;
        for (name, pid) in processes {
            guard.add_process(name, pid);
        }

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
        // and the thread breaks out of the event loop. The WMI `WITHIN 1` poll interval
        // means at most ~1s delay before the thread notices and exits.
        // JoinHandle is intentionally detached — the thread is cleaned up on process exit.
        std::thread::Builder::new()
            .name("wmi-events".into())
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
                let query = "SELECT * FROM __InstanceOperationEvent WITHIN 1 \
                             WHERE TargetInstance ISA 'Win32_Process'";
                let events = match wmi.raw_notification::<ProcessOperationEvent>(query) {
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

                for event_result in events {
                    match event_result {
                        Ok(event) => {
                            let pid = event.target_instance.process_id;
                            match event.class.as_deref() {
                                Some(c) if c.contains("Creation") => {
                                    if let Some(name) = event.target_instance.name {
                                        if event_tx
                                            .blocking_send(ProcessEvent::Created(name, pid))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                                Some(c) if c.contains("Deletion") => {
                                    if event_tx.blocking_send(ProcessEvent::Deleted(pid)).is_err() {
                                        break;
                                    }
                                }
                                _ => {
                                    // Unknown or missing __CLASS (e.g. modification events)
                                    // Skip — 60s reconciliation catches anything missed
                                    debug!("WMI: skipping event with class {:?}", event.class);
                                }
                            }
                        }
                        Err(e) => {
                            error!("WMI process event error: {}", e);
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn WMI events thread");

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
                                        debug!("Process ended: PID {}", pid);
                                        guard.remove_process(pid);
                                    }
                                }
                            }
                            drop(guard);
                            let _ = change_tx.send(ProcessChangeNotification);
                        }
                        None => {
                            // WMI event channel closed — thread died due to error.
                            // Continue with reconciliation-only mode at minimum interval.
                            warn!("WMI event stream lost, falling back to reconciliation every {}s", MIN_RECONCILE_SECS);
                            wmi_alive = false;
                            reconcile_secs = MIN_RECONCILE_SECS;
                            consecutive_clean = 0;
                            next_reconcile = tokio::time::Instant::now() + Duration::from_secs(reconcile_secs);
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_reconcile) => {
                    let (pruned, added) = Self::reconcile(state).await;
                    if pruned > 0 || added > 0 {
                        debug!("Reconciliation: pruned {} stale, added {} new entries, resetting interval to {}s", pruned, added, MIN_RECONCILE_SECS);
                        // Drift detected — shrink interval back to minimum
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
            Ok(s) => s,
            Err(_) => return (0, 0),
        };

        let mut guard = state.write().await;

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

        // Reclaim excess capacity after churn (short-lived processes, installers, etc.)
        guard.pid_to_name.shrink_to(256);
        guard.name_counts.shrink_to(256);
        guard.names.shrink_to(256);

        (pruned, added)
    }

    /// Take a full process snapshot using ToolHelp API
    fn snapshot_all_processes() -> std::collections::HashMap<u32, String> {
        let mut pids = std::collections::HashMap::with_capacity(256);

        // SAFETY: CreateToolhelp32Snapshot and Process32 enumeration are
        // standard Win32 APIs for process listing. Handle is properly closed.
        unsafe {
            if let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                let mut entry = PROCESSENTRY32W {
                    dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };

                if Process32FirstW(snapshot, &raw mut entry).is_ok() {
                    loop {
                        let mut name = String::from_utf16_lossy(&entry.szExeFile);
                        if let Some(pos) = name.find('\0') {
                            name.truncate(pos);
                        }
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

        pids
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
