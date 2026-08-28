//! Module host: runs, supervises and stops external programs.
//!
//! A module is an external program, not linked into this binary. It exists for
//! local automation with no MQTT leg (a hardware control surface driving app
//! volume is local input to local output), and it keeps device-specific personal
//! automation out of this tree entirely.
//!
//! v1 is process supervision only: run it, restart it per policy, capture its
//! output, publish its state. There is deliberately no stdio protocol and no
//! host API. Do not add one until a module needs to call back into pc-bridge.
//!
//! Privileges reuse the custom-command gates rather than a parallel system:
//! `admin: true` requires `custom_command_privileges_allowed` (enforced at
//! config load, like `validate_custom_command`) AND an actually-elevated agent
//! (enforced here, before spawn). `admin` is a gate, never an escalation.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc};

use crate::AppState;
use crate::config::{ModuleDef, RestartPolicy};

/// Backoff bounds for `on-failure` / `always` restarts.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A run lasting at least this long is treated as healthy, so backoff resets.
/// Without it, a module that takes 30s to fail keeps its 60s backoff forever.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);
/// Longest line accepted from a module's stdout/stderr before it is dropped.
/// A module that never emits a newline must not grow the buffer without bound.
const MAX_LINE: usize = 2048;
/// Bound on queued output lines per module. Full means drop with a counted
/// warning: applying backpressure to the child would stall a chatty module
/// against a busy log, which is worse than losing diagnostic lines.
const OUTPUT_QUEUE: usize = 64;

/// What HA sees for each module, in the `modules` sensor's attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleState {
    Running,
    Stopped,
    /// A required privilege is not satisfied. Logged once on entry, never retried
    /// on a timer: re-evaluation happens on config change, which the supervisor
    /// already drives.
    Blocked,
    Failed,
}

impl ModuleState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

/// True when this process is running elevated. Non-Windows has no equivalent
/// gate, so `admin` modules are refused there rather than silently allowed.
#[cfg(windows)]
fn is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: token handle is closed on every path; GetTokenInformation is given
    // the exact size of the TOKEN_ELEVATION it writes into.
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(std::ptr::from_mut(&mut elevation).cast()),
            u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
const fn is_elevated() -> bool {
    false
}

/// Why a module cannot start right now. `None` means it may start.
fn blocked_reason(def: &ModuleDef, privileges_allowed: bool) -> Option<String> {
    if def.admin {
        // Config load already bails on this, so reaching it here means a config
        // was hot-reloaded past validation. Refuse rather than assume.
        if !privileges_allowed {
            return Some("admin=true but custom_command_privileges_allowed=false".to_string());
        }
        if !is_elevated() {
            return Some("admin=true but pc-bridge is not running elevated".to_string());
        }
    }
    None
}

/// Kills a module's whole process TREE when dropped.
///
/// `kill_on_drop` only kills the direct child. A module that is a launcher (a
/// python shim, a shell wrapper) leaves its real worker running, holding the
/// device or port the next start needs. A job object with
/// `KILL_ON_JOB_CLOSE` terminates every descendant when the handle closes.
///
/// Caveat, stated rather than papered over: assignment happens just AFTER
/// `CreateProcess`, so a child that forks within that window escapes the job.
/// Closing that hole needs `CREATE_SUSPENDED` + assign + `ResumeThread`, which
/// `tokio::process::Command` does not expose. The window is microseconds and
/// the failure mode is one stray grandchild, not a leak that compounds.
#[cfg(windows)]
struct JobHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl JobHandle {
    /// Create a kill-on-close job and put `pid` in it. Returns None on any
    /// failure: losing tree teardown is worse than a plain child, but it is not
    /// a reason to refuse to run the module.
    fn assign(pid: u32) -> Option<Self> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        // SAFETY: every handle opened here is closed on every path. The
        // SetInformationJobObject pointer/length pair describes the exact
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION on the stack below.
        unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let sized = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).ok();
            let ok = sized.is_some_and(|n| {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    n,
                )
                .is_ok()
            });
            if !ok {
                let _ = CloseHandle(job);
                return None;
            }
            let Ok(proc) = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) else {
                let _ = CloseHandle(job);
                return None;
            };
            let assigned = AssignProcessToJobObject(job, proc).is_ok();
            let _ = CloseHandle(proc);
            if assigned {
                Some(Self(job))
            } else {
                let _ = CloseHandle(job);
                None
            }
        }
    }
}

/// SAFETY: a job handle is a process-wide kernel handle with no thread affinity.
/// The raw `HANDLE` is only `!Send` because it is a bare pointer. `JobHandle`
/// owns it uniquely and closes it exactly once in `Drop`, so moving it onto
/// another task thread is sound. Needed because the supervisor task holds it
/// across awaits and `tokio::spawn` requires `Send`.
#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: self.0 is a job handle this type created and owns uniquely.
        // Closing the last handle is what triggers KILL_ON_JOB_CLOSE.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// True when a running module must be stopped: it is gone from config, or its
/// definition changed. `previous` is the definition set this manager last acted
/// on, so an unchanged module keeps running and an unrelated settings write does
/// not bounce it.
fn should_stop(name: &str, wanted: &[ModuleDef], previous: &[ModuleDef]) -> bool {
    let Some(new) = wanted.iter().find(|d| d.name == name) else {
        return true; // removed from config
    };
    previous.iter().find(|d| d.name == name) != Some(new)
}

pub struct ModuleManager {
    state: Arc<AppState>,
}

impl ModuleManager {
    pub const fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Takes the per-task shutdown SENDER: this task owns child processes, so
    /// stopping it must also stop them. Firing the sender cancels every module.
    pub async fn run(self, task_shutdown: broadcast::Sender<()>) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut task_rx = task_shutdown.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();
        let (status_tx, mut status_rx) = mpsc::channel::<(String, ModuleState)>(64);

        let mut defs: Vec<ModuleDef> = Vec::new();
        let mut running: HashMap<String, broadcast::Sender<()>> = HashMap::new();
        let mut status: HashMap<String, ModuleState> = HashMap::new();

        self.reload(&mut defs, &mut running, &mut status, &status_tx)
            .await;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => break,
                _ = task_rx.recv() => break,
                r = config_rx.recv() => {
                    // Lagged must reload too: a burst of generations would
                    // otherwise leave stale modules running.
                    if !matches!(
                        r,
                        Ok(()) | Err(broadcast::error::RecvError::Lagged(_))
                    ) {
                        break;
                    }
                    self.reload(&mut defs, &mut running, &mut status, &status_tx).await;
                }
                Some((name, new_state)) = status_rx.recv() => {
                    // Publish on transition only. A steadily running module must
                    // cost zero MQTT traffic.
                    if status.get(&name) != Some(&new_state) {
                        status.insert(name, new_state);
                        self.publish_status(&status).await;
                    }
                }
            }
        }

        for (name, cancel) in &running {
            debug!("Stopping module '{name}'");
            let _ = cancel.send(());
        }
        info!("Module host stopped ({} module(s))", running.len());
    }

    /// Start, stop and restart modules to match the current config. Modules whose
    /// definition is unchanged keep running: restarting everything on any config
    /// write would bounce unrelated modules on every settings tweak.
    async fn reload(
        &self,
        defs: &mut Vec<ModuleDef>,
        running: &mut HashMap<String, broadcast::Sender<()>>,
        status: &mut HashMap<String, ModuleState>,
        status_tx: &mpsc::Sender<(String, ModuleState)>,
    ) {
        let (new_defs, enabled, privileges_allowed) = {
            let cfg = self.state.config.read().await;
            (
                cfg.modules.clone(),
                cfg.modules_enabled,
                cfg.custom_command_privileges_allowed,
            )
        };
        let wanted: Vec<ModuleDef> = if enabled { new_defs } else { Vec::new() };

        // Stop modules that are gone or whose definition changed.
        let stale: Vec<String> = running
            .keys()
            .filter(|name| should_stop(name, &wanted, defs))
            .cloned()
            .collect();
        for name in stale {
            if let Some(cancel) = running.remove(&name) {
                let _ = cancel.send(());
            }
        }

        // Drop status for anything no longer in config. This has to key off
        // `wanted` rather than off the stopped set: a BLOCKED module never
        // entered `running`, so pruning only that map would leave a module the
        // user deleted reporting `blocked` to HA forever.
        status.retain(|name, _| wanted.iter().any(|d| &d.name == name));

        // Start anything wanted that is not already running.
        for def in &wanted {
            if running.contains_key(&def.name) {
                continue;
            }
            if let Some(reason) = blocked_reason(def, privileges_allowed) {
                // Only on transition. `reload` runs on every config generation,
                // so warning unconditionally would re-log for every blocked
                // module every time an unrelated setting is saved.
                if status.get(&def.name) != Some(&ModuleState::Blocked) {
                    warn!(
                        "Module '{}' NOT STARTED - {reason}. Fix the config, or remove admin=true.",
                        def.name
                    );
                }
                status.insert(def.name.clone(), ModuleState::Blocked);
                continue;
            }
            let (cancel_tx, cancel_rx) = broadcast::channel(1);
            running.insert(def.name.clone(), cancel_tx);
            status.insert(def.name.clone(), ModuleState::Running);
            tokio::spawn(supervise_one(def.clone(), status_tx.clone(), cancel_rx));
            info!("Module '{}' started", def.name);
        }

        *defs = wanted;
        self.publish_status(status).await;
    }

    /// One entity, not one per module: modules churn on hot-reload and a
    /// per-module entity would need dynamic discovery registration and teardown.
    async fn publish_status(&self, status: &HashMap<String, ModuleState>) {
        let running = status
            .values()
            .filter(|s| **s == ModuleState::Running)
            .count();
        let attrs: serde_json::Map<String, serde_json::Value> = status
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())))
            .collect();
        self.state
            .mqtt
            .publish_sensor("modules", &running.to_string())
            .await;
        self.state
            .mqtt
            .publish_sensor_attributes("modules", &serde_json::Value::Object(attrs))
            .await;
    }
}

/// Run one module until cancelled, applying its restart policy.
///
/// No polling: this awaits the child's exit, and sleeps only between restarts.
async fn supervise_one(
    def: ModuleDef,
    status_tx: mpsc::Sender<(String, ModuleState)>,
    mut cancel: broadcast::Receiver<()>,
) {
    let mut backoff = BACKOFF_MIN;

    loop {
        let started = tokio::time::Instant::now();
        let outcome = run_child(&def, &mut cancel).await;
        let ran_for = started.elapsed();

        match outcome {
            ChildOutcome::Cancelled => {
                let _ = status_tx
                    .send((def.name.clone(), ModuleState::Stopped))
                    .await;
                return;
            }
            ChildOutcome::SpawnFailed(e) => {
                error!("Module '{}' failed to start: {e}", def.name);
                let _ = status_tx
                    .send((def.name.clone(), ModuleState::Failed))
                    .await;
                if def.restart == RestartPolicy::Never {
                    return;
                }
            }
            ChildOutcome::Exited { success } => {
                if success {
                    info!("Module '{}' exited cleanly", def.name);
                } else {
                    warn!("Module '{}' exited with failure", def.name);
                }
                let restart = match def.restart {
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => !success,
                    RestartPolicy::Never => false,
                };
                let _ = status_tx
                    .send((
                        def.name.clone(),
                        if restart {
                            ModuleState::Running
                        } else if success {
                            ModuleState::Stopped
                        } else {
                            ModuleState::Failed
                        },
                    ))
                    .await;
                if !restart {
                    return;
                }
                if ran_for >= BACKOFF_RESET_AFTER {
                    backoff = BACKOFF_MIN;
                }
            }
        }

        debug!("Module '{}' restarting in {:?}", def.name, backoff);
        tokio::select! {
            biased;
            _ = cancel.recv() => {
                let _ = status_tx.send((def.name.clone(), ModuleState::Stopped)).await;
                return;
            }
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

enum ChildOutcome {
    Cancelled,
    SpawnFailed(std::io::Error),
    Exited { success: bool },
}

/// Spawn the module and wait for it to exit or be cancelled.
async fn run_child(def: &ModuleDef, cancel: &mut broadcast::Receiver<()>) -> ChildOutcome {
    let mut cmd = tokio::process::Command::new(&def.command);
    cmd.args(&def.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Cancel drops this future; kill_on_drop then terminates the child rather
        // than detaching it. Without it, disabling the feature leaks a process.
        .kill_on_drop(true);
    if let Some(dir) = &def.working_dir {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        // No console window for a background module. This is tokio's own
        // `creation_flags`, not the std `CommandExt` one.
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ChildOutcome::SpawnFailed(e),
    };

    // Held for the child's lifetime; dropping it kills the tree. Bound BEFORE
    // any early return below, so cancellation on any path takes the tree down.
    #[cfg(windows)]
    let job = child.id().and_then(JobHandle::assign);
    #[cfg(windows)]
    if job.is_none() {
        debug!(
            "Module '{}' is not in a job object; only the direct child will be killed on stop",
            def.name
        );
    }

    let name = def.name.clone();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(OUTPUT_QUEUE);
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(stdout, out_tx.clone(), name.clone(), false));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(stderr, out_tx, name.clone(), true));
    }

    loop {
        tokio::select! {
            biased;
            _ = cancel.recv() => return ChildOutcome::Cancelled,
            Some(line) = out_rx.recv() => info!("[{name}] {line}"),
            status = child.wait() => {
                return match status {
                    Ok(s) => ChildOutcome::Exited { success: s.success() },
                    Err(e) => ChildOutcome::SpawnFailed(e),
                };
            }
        }
    }
}

/// Read a child pipe into the log channel, one line at a time.
///
/// Deliberately NOT `BufReader::lines()`. That calls `read_until`, which grows
/// its buffer until it finds a newline, so a module emitting megabytes with no
/// `\n` is buffered in full before any length check could reject it. This reads
/// fixed chunks into an accumulator capped at `MAX_LINE` and, once a line goes
/// over, discards the rest of it as it arrives. Peak memory is bounded by the
/// cap regardless of what the child writes.
///
/// A full channel drops rather than applying backpressure: stalling a chatty
/// module against a busy log is worse than losing diagnostic lines.
async fn pump<R>(reader: R, tx: mpsc::Sender<String>, name: String, is_err: bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut reader = reader;
    let mut chunk = [0u8; 4096];
    let mut line: Vec<u8> = Vec::with_capacity(256);
    // True once the current line passed the cap: swallow bytes until the newline.
    let mut overlong = false;
    let mut dropped: u64 = 0;

    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        for &b in &chunk[..n] {
            if b == b'\n' {
                if overlong {
                    dropped += 1;
                    overlong = false;
                } else if !emit(&line, &tx, is_err) {
                    dropped += 1;
                }
                line.clear();
            } else if overlong {
                // Still inside a line that already blew the cap.
            } else if line.len() == MAX_LINE {
                overlong = true;
                line.clear();
            } else {
                line.push(b);
            }
        }
    }

    // A child that exits without a trailing newline still has something to say.
    if !overlong && !line.is_empty() && !emit(&line, &tx, is_err) {
        dropped += 1;
    }
    if dropped > 0 {
        warn!("Module '{name}' dropped {dropped} output line(s)");
    }
}

/// Queue one line. Returns false if it was dropped (full channel or closed).
fn emit(line: &[u8], tx: &mpsc::Sender<String>, is_err: bool) -> bool {
    // Trim a trailing CR so CRLF output does not log a stray carriage return.
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = String::from_utf8_lossy(line);
    let text = if is_err {
        format!("stderr: {text}")
    } else {
        text.into_owned()
    };
    tx.try_send(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RestartPolicy;

    fn def(name: &str, command: &str, args: &[&str], restart: RestartPolicy) -> ModuleDef {
        ModuleDef {
            name: name.to_string(),
            command: command.to_string(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            working_dir: None,
            restart,
            admin: false,
        }
    }

    #[test]
    fn non_admin_module_is_never_blocked() {
        let m = def("m", "true", &[], RestartPolicy::Never);
        assert!(blocked_reason(&m, false).is_none());
        assert!(blocked_reason(&m, true).is_none());
    }

    #[test]
    fn admin_module_blocked_without_privileges_allowed() {
        let mut m = def("m", "true", &[], RestartPolicy::Never);
        m.admin = true;
        let reason = blocked_reason(&m, false).expect("must be blocked");
        assert!(reason.contains("custom_command_privileges_allowed"));
    }

    /// The whole point of the gate: `admin: true` never escalates. With
    /// privileges allowed but the agent unelevated, the module must not run.
    #[test]
    fn admin_module_blocked_when_agent_is_not_elevated() {
        let mut m = def("m", "true", &[], RestartPolicy::Never);
        m.admin = true;
        if is_elevated() {
            assert!(blocked_reason(&m, true).is_none());
        } else {
            let reason = blocked_reason(&m, true).expect("must be blocked");
            assert!(reason.contains("not running elevated"));
        }
    }

    #[test]
    fn unchanged_module_keeps_running_across_a_reload() {
        // An unrelated settings write must not bounce every running module.
        let a = def("a", "true", &[], RestartPolicy::Never);
        let b = def("b", "true", &[], RestartPolicy::Never);
        let wanted = vec![a.clone(), b.clone()];
        let previous = vec![a, b];
        assert!(!should_stop("a", &wanted, &previous));
        assert!(!should_stop("b", &wanted, &previous));
    }

    #[test]
    fn changed_or_removed_modules_stop() {
        let a = def("a", "true", &[], RestartPolicy::Never);
        let mut a2 = a.clone();
        a2.args = vec!["--now".to_string()];

        // Redefined: must stop, so the start loop respawns it with the new def.
        assert!(should_stop("a", &[a2], std::slice::from_ref(&a)));
        // Gone from config entirely.
        assert!(should_stop("a", &[], std::slice::from_ref(&a)));
        // Never seen before: nothing to stop, but nothing running either.
        assert!(should_stop("ghost", &[a], &[]));
    }

    /// Regression: a BLOCKED module never enters the running map, so pruning
    /// only that map left a deleted module reporting `blocked` to HA forever.
    #[test]
    fn status_prunes_modules_that_left_the_config() {
        let a = def("a", "true", &[], RestartPolicy::Never);
        let mut status: HashMap<String, ModuleState> = HashMap::new();
        status.insert("a".to_string(), ModuleState::Running);
        status.insert("blocked-and-deleted".to_string(), ModuleState::Blocked);

        let wanted = [a];
        status.retain(|name, _| wanted.iter().any(|d| &d.name == name));

        assert_eq!(status.get("a"), Some(&ModuleState::Running));
        assert!(
            !status.contains_key("blocked-and-deleted"),
            "a module removed from config must not keep reporting a state"
        );
    }

    #[test]
    fn state_strings_are_stable() {
        // These land in MQTT attributes that HA templates match on.
        assert_eq!(ModuleState::Running.as_str(), "running");
        assert_eq!(ModuleState::Stopped.as_str(), "stopped");
        assert_eq!(ModuleState::Blocked.as_str(), "blocked");
        assert_eq!(ModuleState::Failed.as_str(), "failed");
    }

    #[tokio::test]
    async fn pump_drops_overlong_lines_but_keeps_short_ones() {
        let long = "x".repeat(MAX_LINE + 1);
        let input = format!("ok one\n{long}\nok two\n");
        let (tx, mut rx) = mpsc::channel(8);
        pump(
            std::io::Cursor::new(input.into_bytes()),
            tx,
            "t".to_string(),
            false,
        )
        .await;
        assert_eq!(rx.recv().await.as_deref(), Some("ok one"));
        assert_eq!(rx.recv().await.as_deref(), Some("ok two"));
        assert!(rx.recv().await.is_none());
    }

    /// The regression this reader exists for: `BufReader::lines()` would buffer
    /// all 4 MB before any cap could reject it. Nothing here may be retained.
    #[tokio::test]
    async fn a_huge_line_with_no_newline_is_bounded() {
        let mut input = vec![b'x'; 4 * 1024 * 1024];
        input.extend_from_slice(b"\nsurvivor\n");
        let (tx, mut rx) = mpsc::channel(8);
        pump(std::io::Cursor::new(input), tx, "t".to_string(), false).await;
        // The 4 MB line is gone; the line after it still arrives.
        assert_eq!(rx.recv().await.as_deref(), Some("survivor"));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_line_of_exactly_max_len_survives() {
        let line = "y".repeat(MAX_LINE);
        let (tx, mut rx) = mpsc::channel(8);
        pump(
            std::io::Cursor::new(format!("{line}\n").into_bytes()),
            tx,
            "t".to_string(),
            false,
        )
        .await;
        assert_eq!(rx.recv().await.as_deref(), Some(line.as_str()));
    }

    #[tokio::test]
    async fn crlf_output_does_not_log_a_stray_carriage_return() {
        // Windows modules are the common case, so this is the common case.
        let (tx, mut rx) = mpsc::channel(8);
        pump(
            std::io::Cursor::new(b"one\r\ntwo\r\n".to_vec()),
            tx,
            "t".to_string(),
            false,
        )
        .await;
        assert_eq!(rx.recv().await.as_deref(), Some("one"));
        assert_eq!(rx.recv().await.as_deref(), Some("two"));
    }

    #[tokio::test]
    async fn a_final_line_without_a_newline_is_not_lost() {
        let (tx, mut rx) = mpsc::channel(8);
        pump(
            std::io::Cursor::new(b"no trailing newline".to_vec()),
            tx,
            "t".to_string(),
            false,
        )
        .await;
        assert_eq!(rx.recv().await.as_deref(), Some("no trailing newline"));
    }

    #[tokio::test]
    async fn invalid_utf8_does_not_kill_the_pump() {
        let (tx, mut rx) = mpsc::channel(8);
        pump(
            std::io::Cursor::new(b"bad \xff\xfe byte\ngood\n".to_vec()),
            tx,
            "t".to_string(),
            false,
        )
        .await;
        assert!(rx.recv().await.expect("lossy line").contains("byte"));
        assert_eq!(rx.recv().await.as_deref(), Some("good"));
    }

    /// CPU cost of the module host, measured rather than asserted.
    ///
    /// `#[ignore]`d: it burns wall-clock and prints numbers rather than
    /// asserting a threshold, which would be flaky on shared CI runners.
    /// Run with `cargo test --release -- --ignored --nocapture module_host_cpu`.
    /// Use --release: a debug build roughly triples the per-line cost and is not
    /// what ships.
    ///
    /// The PUMP figure is a CEILING (how fast the reader can go when saturated),
    /// not a running cost. IDLE and CHATTY are the numbers that describe what a
    /// module actually costs.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "measurement, not a pass/fail test"]
    async fn module_host_cpu_cost() {
        const LINES: usize = 200_000;

        fn cpu_micros() -> u64 {
            // SAFETY: getrusage writes a fully-initialised rusage; RUSAGE_SELF
            // is a valid who value and the pointer is to a live local.
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut ru) };
            let f = |t: libc::timeval| {
                u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000
                    + u64::try_from(t.tv_usec).unwrap_or(0)
            };
            f(ru.ru_utime) + f(ru.ru_stime)
        }

        // 1. A silent, running module over 5 seconds.
        let (tx, _rx) = mpsc::channel(8);
        let (cancel_tx, cancel_rx) = broadcast::channel(1);
        let m = def(
            "idle",
            "/bin/sh",
            &["-c", "sleep 60"],
            RestartPolicy::Always,
        );
        let h = tokio::spawn(supervise_one(m, tx, cancel_rx));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = cpu_micros();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let idle_cost = cpu_micros() - before;
        let _ = cancel_tx.send(());
        let _ = h.await;
        println!(
            "IDLE  5s with a running module: {idle_cost} us CPU ({:.6}% of one core)",
            idle_cost as f64 / 50_000.0
        );

        // 2. A realistic chatty module: 10 lines/s for 5 seconds. This is the
        //    number that answers "what does it cost me", not the ceiling below.
        let (tx, mut rx) = mpsc::channel(8);
        let (cancel_tx, cancel_rx) = broadcast::channel(1);
        // One self-pacing process. A shell loop would fork a `sleep` per line and
        // charge that scheduler noise to this measurement instead of the pump.
        let m = def(
            "chatty",
            "/usr/bin/python3",
            &[
                "-c",
                "import time\nfor i in range(50):\n print('tick', i, flush=True)\n time.sleep(0.1)",
            ],
            RestartPolicy::Never,
        );
        let h = tokio::spawn(supervise_one(m, tx, cancel_rx));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = cpu_micros();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let chatty_cost = cpu_micros() - before;
        let _ = cancel_tx.send(());
        while rx.try_recv().is_ok() {}
        let _ = h.await;
        println!(
            "CHATTY 5s at ~10 lines/s: {chatty_cost} us CPU ({:.6}% of one core)",
            chatty_cost as f64 / 50_000.0
        );

        // 3. Ceiling of the output pump. NOT a running cost: this is how fast it
        //    CAN go when saturated, which only happens if a module floods it.
        println!("--- the number below is a CEILING, not a steady-state cost ---");
        let payload = "a module log line of roughly typical length\n".repeat(LINES);
        let bytes = payload.len();
        let (tx, rx) = mpsc::channel(OUTPUT_QUEUE);
        // NOTE: reading a Cursor never yields, so the pump runs to completion
        // before this task is scheduled and `delivered` comes out near the queue
        // depth. That is not a flaw in the measurement, it IS the burst
        // behaviour: a module that outruns the log gets dropped, not throttled.
        // Real pipes await on read, so a steady producer is delivered normally.
        let drain = tokio::spawn(async move {
            let mut rx = rx;
            let mut n = 0u64;
            while rx.recv().await.is_some() {
                n += 1;
            }
            n
        });
        let before = cpu_micros();
        let t = tokio::time::Instant::now();
        pump(
            std::io::Cursor::new(payload.into_bytes()),
            tx,
            "bench".to_string(),
            false,
        )
        .await;
        let wall = t.elapsed();
        let cost = cpu_micros() - before;
        let delivered = drain.await.unwrap_or(0);
        println!(
            "PUMP  {LINES} lines / {:.1} MB in {wall:?} ({cost} us CPU), {delivered} delivered, \
             {:.0} lines/s, {:.1} MB/s",
            bytes as f64 / 1_048_576.0,
            LINES as f64 / wall.as_secs_f64(),
            bytes as f64 / 1_048_576.0 / wall.as_secs_f64(),
        );
    }

    #[tokio::test]
    async fn stderr_lines_are_tagged() {
        let (tx, mut rx) = mpsc::channel(8);
        pump(
            std::io::Cursor::new(b"boom\n".to_vec()),
            tx,
            "t".to_string(),
            true,
        )
        .await;
        assert_eq!(rx.recv().await.as_deref(), Some("stderr: boom"));
    }

    #[tokio::test]
    async fn spawn_failure_with_restart_never_gives_up() {
        let (tx, mut rx) = mpsc::channel(8);
        let (_cancel_tx, cancel_rx) = broadcast::channel(1);
        let m = def(
            "ghost",
            "definitely-not-a-real-program-4f2a",
            &[],
            RestartPolicy::Never,
        );
        // Must RETURN, not spin: a missing binary with restart=never is terminal.
        supervise_one(m, tx, cancel_rx).await;
        assert_eq!(
            rx.recv().await,
            Some(("ghost".to_string(), ModuleState::Failed))
        );
        assert!(rx.recv().await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clean_exit_with_on_failure_does_not_restart() {
        let (tx, mut rx) = mpsc::channel(8);
        let (_cancel_tx, cancel_rx) = broadcast::channel(1);
        let m = def(
            "done",
            "/bin/sh",
            &["-c", "exit 0"],
            RestartPolicy::OnFailure,
        );
        supervise_one(m, tx, cancel_rx).await;
        assert_eq!(
            rx.recv().await,
            Some(("done".to_string(), ModuleState::Stopped))
        );
        assert!(rx.recv().await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_with_on_failure_restarts_and_cancel_wins_during_backoff() {
        let (tx, mut rx) = mpsc::channel(8);
        let (cancel_tx, cancel_rx) = broadcast::channel(1);
        let m = def(
            "flaky",
            "/bin/sh",
            &["-c", "exit 3"],
            RestartPolicy::OnFailure,
        );
        let h = tokio::spawn(supervise_one(m, tx, cancel_rx));
        // Restart intent is reported as Running before the backoff sleep.
        assert_eq!(
            rx.recv().await,
            Some(("flaky".to_string(), ModuleState::Running))
        );
        // Cancelling mid-backoff must return promptly, not wait out the sleep.
        let _ = cancel_tx.send(());
        assert_eq!(
            rx.recv().await,
            Some(("flaky".to_string(), ModuleState::Stopped))
        );
        h.await.expect("supervisor task must not panic");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_stops_a_running_module() {
        let (tx, mut rx) = mpsc::channel(8);
        let (cancel_tx, cancel_rx) = broadcast::channel(1);
        let m = def(
            "sleeper",
            "/bin/sh",
            &["-c", "sleep 30"],
            RestartPolicy::Always,
        );
        let h = tokio::spawn(supervise_one(m, tx, cancel_rx));
        tokio::task::yield_now().await;
        let _ = cancel_tx.send(());
        assert_eq!(
            rx.recv().await,
            Some(("sleeper".to_string(), ModuleState::Stopped))
        );
        // Not just "reported stopped": the task must actually finish.
        tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("must not hang on cancel")
            .expect("supervisor task must not panic");
    }
}
