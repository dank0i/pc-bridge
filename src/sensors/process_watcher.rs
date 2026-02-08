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

use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use wmi::{COMLibrary, WMIConnection};

/// Notification sent when process list changes
#[derive(Clone, Debug)]
pub struct ProcessChangeNotification;

/// Process creation event from WMI
#[derive(Deserialize, Debug)]
#[serde(rename = "__InstanceCreationEvent")]
#[serde(rename_all = "PascalCase")]
struct ProcessCreationEvent {
    target_instance: Win32Process,
}

/// Process deletion event from WMI
#[derive(Deserialize, Debug)]
#[serde(rename = "__InstanceDeletionEvent")]
#[serde(rename_all = "PascalCase")]
struct ProcessDeletionEvent {
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
    names: HashSet<String>,
    /// Process ID to name mapping (for deletion lookup)
    pid_to_name: std::collections::HashMap<u32, String>,
    /// Last update time (for diagnostics)
    last_updated: Instant,
}

impl ProcessState {
    fn new() -> Self {
        Self {
            names: HashSet::with_capacity(256),
            pid_to_name: std::collections::HashMap::with_capacity(256),
            last_updated: Instant::now(),
        }
    }

    fn add_process(&mut self, name: String, pid: u32) {
        self.pid_to_name.insert(pid, name.clone());
        self.names.insert(name);
        self.last_updated = Instant::now();
    }

    fn remove_process(&mut self, pid: u32) {
        if let Some(name) = self.pid_to_name.remove(&pid) {
            // Only remove from names if no other process has the same name
            let still_exists = self.pid_to_name.values().any(|n| n == &name);
            if !still_exists {
                self.names.remove(&name);
            }
            self.last_updated = Instant::now();
        }
    }
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
        // Channel capacity of 16 is enough - subscribers just need to know "something changed"
        let (change_tx, _) = broadcast::channel(16);

        // Initial enumeration using ToolHelp (fast, reliable)
        Self::initial_enumeration(&state).await;

        Self { state, change_tx }
    }

    /// Subscribe to process change notifications
    pub fn subscribe(&self) -> broadcast::Receiver<ProcessChangeNotification> {
        self.change_tx.subscribe()
    }

    /// Notify subscribers that processes changed
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

        // Try WMI first, fall back to polling
        tokio::spawn(async move {
            match Self::setup_wmi_events(&state, change_tx.clone()).await {
                Ok(()) => {
                    info!("Process watcher using WMI events");
                    // WMI threads are running, just wait for shutdown
                    let mut rx = shutdown_rx;
                    let _ = rx.recv().await;
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

    /// Perform initial process enumeration using ToolHelp API
    async fn initial_enumeration(state: &Arc<RwLock<ProcessState>>) {
        let mut guard = state.write().await;

        unsafe {
            if let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                let mut entry = PROCESSENTRY32W {
                    dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };

                if Process32FirstW(snapshot, &mut entry).is_ok() {
                    loop {
                        // Extract process name
                        let mut name = String::from_utf16_lossy(&entry.szExeFile);
                        if let Some(pos) = name.find('\0') {
                            name.truncate(pos);
                        }

                        if !name.is_empty() {
                            guard.add_process(name, entry.th32ProcessID);
                        }

                        if Process32NextW(snapshot, &mut entry).is_err() {
                            break;
                        }
                    }
                }

                let _ = CloseHandle(snapshot);
            }
        }

        info!(
            "Initial process enumeration: {} processes",
            guard.names.len()
        );
    }

    /// Set up WMI event subscription for process changes
    async fn setup_wmi_events(
        state: &Arc<RwLock<ProcessState>>,
        change_tx: broadcast::Sender<ProcessChangeNotification>,
    ) -> anyhow::Result<()> {
        // WMI operations are blocking, run in spawn_blocking
        let state = Arc::clone(state);

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            // Initialize COM for this thread
            let com = COMLibrary::new()?;
            let wmi = WMIConnection::new(com)?;

            info!("WMI connection established, subscribing to process events");

            // Create subscription queries
            // WITHIN 1 = check every 1 second for events
            let creation_query = "SELECT * FROM __InstanceCreationEvent WITHIN 1 WHERE TargetInstance ISA 'Win32_Process'";
            let deletion_query = "SELECT * FROM __InstanceDeletionEvent WITHIN 1 WHERE TargetInstance ISA 'Win32_Process'";

            // WMI uses blocking iterators, we need separate threads for each
            let state_creation = Arc::clone(&state);
            let state_deletion = Arc::clone(&state);
            let change_tx_creation = change_tx.clone();
            let change_tx_deletion = change_tx;

            // Spawn thread for creation events
            std::thread::spawn(move || {
                let com = match COMLibrary::new() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to init COM for creation thread: {}", e);
                        return;
                    }
                };
                let wmi = match WMIConnection::new(com) {
                    Ok(w) => w,
                    Err(e) => {
                        error!("Failed to create WMI connection for creation: {}", e);
                        return;
                    }
                };

                debug!("WMI process creation event thread started");

                let events = match wmi.raw_notification::<ProcessCreationEvent>(creation_query) {
                    Ok(iter) => iter,
                    Err(e) => {
                        error!("Failed to subscribe to creation events: {}", e);
                        return;
                    }
                };

                for event_result in events {
                    match event_result {
                        Ok(event) => {
                            if let Some(name) = event.target_instance.name {
                                let pid = event.target_instance.process_id;
                                debug!("Process started: {} (PID {})", name, pid);

                                // Use blocking lock since we're in a sync context
                                if let Ok(mut guard) = state_creation.try_write() {
                                    guard.add_process(name, pid);
                                } else {
                                    // Try again with a short wait
                                    std::thread::sleep(Duration::from_millis(10));
                                    let rt = tokio::runtime::Handle::try_current();
                                    if let Ok(handle) = rt {
                                        handle.block_on(async {
                                            state_creation.write().await.add_process(name, pid);
                                        });
                                    }
                                }
                                // Notify subscribers of change
                                let _ = change_tx_creation.send(ProcessChangeNotification);
                            }
                        }
                        Err(e) => {
                            // Connection lost or query error
                            error!("WMI creation event error: {}", e);
                            break;
                        }
                    }
                }
            });

            // Spawn thread for deletion events
            std::thread::spawn(move || {
                let com = match COMLibrary::new() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to init COM for deletion thread: {}", e);
                        return;
                    }
                };
                let wmi = match WMIConnection::new(com) {
                    Ok(w) => w,
                    Err(e) => {
                        error!("Failed to create WMI connection for deletion: {}", e);
                        return;
                    }
                };

                debug!("WMI process deletion event thread started");

                let events = match wmi.raw_notification::<ProcessDeletionEvent>(deletion_query) {
                    Ok(iter) => iter,
                    Err(e) => {
                        error!("Failed to subscribe to deletion events: {}", e);
                        return;
                    }
                };

                for event_result in events {
                    match event_result {
                        Ok(event) => {
                            let pid = event.target_instance.process_id;
                            debug!("Process ended: {:?} (PID {})", event.target_instance.name, pid);

                            if let Ok(mut guard) = state_deletion.try_write() {
                                guard.remove_process(pid);
                            } else {
                                std::thread::sleep(Duration::from_millis(10));
                                let rt = tokio::runtime::Handle::try_current();
                                if let Ok(handle) = rt {
                                    handle.block_on(async {
                                        state_deletion.write().await.remove_process(pid);
                                    });
                                }
                            }
                            // Notify subscribers of change
                            let _ = change_tx_deletion.send(ProcessChangeNotification);
                        }
                        Err(e) => {
                            error!("WMI deletion event error: {}", e);
                            break;
                        }
                    }
                }
            });

            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Polling fallback if WMI events aren't available
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
                _ = shutdown_rx.recv() => {
                    debug!("Process watcher (polling) shutting down");
                    break;
                }
                _ = interval.tick() => {
                    Self::initial_enumeration(&state).await;
                    // Notify subscribers after each poll
                    let _ = change_tx.send(ProcessChangeNotification);
                }
            }
        }
    }

    /// Get a snapshot of current process names
    pub async fn get_names(&self) -> HashSet<String> {
        self.state.read().await.names.clone()
    }

    /// Check if any screensaver process is running
    pub async fn has_screensaver_running(&self) -> bool {
        self.state
            .read()
            .await
            .names
            .iter()
            .any(|name| name.to_lowercase().ends_with(".scr"))
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
}
