//! HWiNFO supervisor (Windows).
//!
//! When `features.manage_hwinfo` is enabled, pc-bridge keeps HWiNFO running. If
//! one is already running (you started it, or a service did) it *adopts* that
//! instance — never launching a duplicate or killing something it didn't start.
//! Otherwise it launches HWiNFO **onto a hidden desktop** it creates: the process
//! runs normally (driver, sensor polling, `Global\HWiNFO_SENS_SM2` shared memory
//! — all desktop-independent) but its `Shell_NotifyIcon` call can't find the
//! taskbar on that desktop, so **no tray icon appears**.
//!
//! HWiNFO needs admin for its driver, so pc-bridge must itself be elevated for
//! the launch to succeed (a non-elevated launch fails with ERROR_ELEVATION_
//! REQUIRED). Run pc-bridge from an elevated logon task; see the README.
//! Requires HWiNFO Pro for unattended use (Free disables shared memory after
//! ~12h and we don't work around that). Path defaults to `HWiNFO64.exe` next to
//! the pc-bridge binary (override with `PC_BRIDGE_HWINFO_PATH`).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::broadcast;
use tokio::time::{MissedTickBehavior, interval};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CONTROL_FLAGS, HDESK,
};
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::AppState;

/// Executable launched when no explicit path is configured.
const HWINFO_EXE: &str = "HWiNFO64.exe";
/// Name of the off-screen desktop HWiNFO is launched onto.
const DESKTOP_NAME: &str = "pcbridge_hwinfo";
/// GENERIC_ALL — full access for the desktop we create.
const GENERIC_ALL: u32 = 0x1000_0000;
/// How often the child's liveness (or an adopted instance's presence) is polled.
const POLL_SECS: u64 = 5;
/// Backoff before relaunching after our owned instance exits or fails to spawn.
const RESPAWN_BACKOFF_SECS: u64 = 5;

pub struct HwInfoSupervisor {
    state: Arc<AppState>,
}

impl HwInfoSupervisor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        if !self.state.config.read().await.features.manage_hwinfo {
            return;
        }

        let exe = resolve_exe_path();
        let exe_name = exe
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| HWINFO_EXE.to_lowercase());
        let work_dir = exe.parent().map(Path::to_path_buf);

        if !crate::elevation::is_elevated() {
            warn!(
                "HWiNFO supervisor: pc-bridge is not elevated; launching HWiNFO will fail \
                 (its driver needs admin). Run pc-bridge from an elevated logon task — see README. \
                 Will still adopt an HWiNFO started elsewhere."
            );
        }

        // Create the hidden desktop once and hold it open for our lifetime.
        let desktop = match HiddenDesktop::create(DESKTOP_NAME) {
            Ok(d) => {
                info!(
                    "HWiNFO supervisor started (hidden desktop '{DESKTOP_NAME}', target {exe:?})"
                );
                Some(d)
            }
            Err(e) => {
                error!(
                    "Failed to create hidden desktop ({e}); HWiNFO would show a tray icon. Supervising adopt-only."
                );
                None
            }
        };

        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        loop {
            // Already running (you, or a service)? Adopt it — don't duplicate or kill it.
            if is_process_running(&exe_name) {
                info!(
                    "HWiNFO already running; supervising existing instance (not launching a copy)"
                );
                if watch_until_gone(&mut shutdown_rx, &exe_name).await {
                    return;
                }
                warn!("Externally-started HWiNFO disappeared; taking over");
                continue;
            }

            // Need the hidden desktop to launch headless; without it, don't spawn
            // a tray-visible copy — just wait for one to appear.
            let Some(desk) = desktop.as_ref() else {
                if sleep_or_shutdown(&mut shutdown_rx, POLL_SECS).await {
                    return;
                }
                continue;
            };

            let child = match desk.launch(&exe, work_dir.as_deref()) {
                Ok(c) => {
                    info!("HWiNFO launched headless (PID {})", c.pid);
                    c
                }
                Err(e) => {
                    error!("Failed to launch HWiNFO ({exe:?}): {e}");
                    if sleep_or_shutdown(&mut shutdown_rx, RESPAWN_BACKOFF_SECS).await {
                        return;
                    }
                    continue;
                }
            };

            let mut poll = interval(Duration::from_secs(POLL_SECS));
            poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        debug!("HWiNFO supervisor shutting down; terminating the HWiNFO we launched");
                        child.kill();
                        return;
                    }
                    _ = poll.tick() => {
                        if child.has_exited() {
                            warn!("HWiNFO exited; will relaunch");
                            break;
                        }
                    }
                }
            }

            if sleep_or_shutdown(&mut shutdown_rx, RESPAWN_BACKOFF_SECS).await {
                return;
            }
        }
    }
}

/// Resolve the HWiNFO executable: `PC_BRIDGE_HWINFO_PATH` override, else
/// `HWiNFO64.exe` next to the pc-bridge executable.
fn resolve_exe_path() -> PathBuf {
    if let Ok(p) = std::env::var("PC_BRIDGE_HWINFO_PATH")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(HWINFO_EXE)))
        .unwrap_or_else(|| PathBuf::from(HWINFO_EXE))
}

/// An off-screen desktop plus its (wide, NUL-terminated) name for `lpDesktop`.
struct HiddenDesktop {
    handle: HDESK,
    name_w: Vec<u16>,
}

impl HiddenDesktop {
    fn create(name: &str) -> windows::core::Result<Self> {
        let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateDesktopW(
                PCWSTR(name_w.as_ptr()),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS::default(),
                GENERIC_ALL,
                None,
            )?
        };
        Ok(Self { handle, name_w })
    }

    /// Launch `exe` on this desktop. The child inherits pc-bridge's token (so it
    /// is elevated, which HWiNFO's driver requires) but renders on the hidden
    /// desktop, so no tray icon registers.
    fn launch(&self, exe: &Path, work_dir: Option<&Path>) -> windows::core::Result<ChildProc> {
        let exe_w: Vec<u16> = exe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dir_w: Option<Vec<u16>> = work_dir.map(|d| {
            d.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        });
        let mut name_w = self.name_w.clone();

        let si = STARTUPINFOW {
            cb: core::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(name_w.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        unsafe {
            CreateProcessW(
                PCWSTR(exe_w.as_ptr()),
                PWSTR::null(),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(0),
                None,
                dir_w
                    .as_ref()
                    .map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
                &raw const si,
                &raw mut pi,
            )?;
            let _ = CloseHandle(pi.hThread);
            Ok(ChildProc {
                process: pi.hProcess,
                pid: pi.dwProcessId,
            })
        }
    }
}

impl Drop for HiddenDesktop {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseDesktop(self.handle);
        }
    }
}

// SAFETY: HDESK is a raw handle used only from the single supervisor task; tokio
// may move that task across worker threads, so we mark it Send. It is never
// shared or used concurrently.
unsafe impl Send for HiddenDesktop {}

/// A launched process we own: poll [`ChildProc::has_exited`], end with
/// [`ChildProc::kill`]. The handle is closed on drop.
struct ChildProc {
    process: HANDLE,
    pid: u32,
}

impl ChildProc {
    fn has_exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    fn kill(&self) {
        unsafe {
            let _ = TerminateProcess(self.process, 1);
        }
    }
}

impl Drop for ChildProc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.process);
        }
    }
}

// SAFETY: the process HANDLE is owned and touched only by the single supervisor
// task; marking Send lets tokio move that task across worker threads.
unsafe impl Send for ChildProc {}

/// Poll until the named process is gone. Returns `true` if a shutdown signal
/// arrived first (caller should stop), `false` once the process disappears.
async fn watch_until_gone(shutdown_rx: &mut broadcast::Receiver<()>, exe_name: &str) -> bool {
    let mut poll = interval(Duration::from_secs(POLL_SECS));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => return true,
            _ = poll.tick() => {
                if !is_process_running(exe_name) {
                    return false;
                }
            }
        }
    }
}

/// Sleep for `secs`, returning early with `true` if a shutdown signal arrives.
async fn sleep_or_shutdown(shutdown_rx: &mut broadcast::Receiver<()>, secs: u64) -> bool {
    tokio::select! {
        biased;
        _ = shutdown_rx.recv() => true,
        () = tokio::time::sleep(Duration::from_secs(secs)) => false,
    }
}

/// Is a process with this (lowercased) exe name currently running?
fn is_process_running(exe_name_lower: &str) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        let mut found = false;
        if Process32FirstW(snapshot, &raw mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_lowercase();
                if name == exe_name_lower {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &raw mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
        found
    }
}
