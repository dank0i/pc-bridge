//! Shared Linux `/proc` process resolution.
//!
//! Extracted so game detection, the custom `ProcessExists` sensor, and the
//! close/kill command path all share ONE name-resolution rule. The subtlety
//! being centralized: `/proc/<pid>/comm` is truncated to 15 bytes
//! (TASK_COMM_LEN-1), so a longer executable name (e.g. `MarvelRivals_Shipping`,
//! or a Proton game whose comm is `GTA5.exe`) never matches on comm alone. When
//! comm is at the truncation length we prefer the untruncated basename from
//! `cmdline` argv0.

use std::path::Path;

/// One live process: its PID and resolved image name.
pub(crate) struct ProcEntry {
    pub pid: u32,
    pub name: String,
}

/// Resolve the image name for a single `/proc/<pid>` directory, applying the
/// comm-truncation fallback. Returns None when neither comm nor cmdline yields a
/// non-empty name.
pub(crate) fn resolve_name(pid_dir: &Path) -> Option<String> {
    let comm = std::fs::read_to_string(pid_dir.join("comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let name = if comm.len() >= 15 {
        std::fs::read_to_string(pid_dir.join("cmdline"))
            .ok()
            .and_then(|cl| {
                cl.split('\0')
                    .next()
                    .filter(|a| !a.is_empty())
                    .map(|a0| a0.rsplit(['/', '\\']).next().unwrap_or(a0).to_string())
            })
            .filter(|b| !b.is_empty())
            .unwrap_or(comm)
    } else {
        comm
    };
    if name.is_empty() { None } else { Some(name) }
}

/// Resolve every process under `proc_root` (normally `/proc`) to (pid, name).
/// `proc_root` is injectable so the resolution rule can be unit-tested against a
/// fake `/proc` layout.
pub(crate) fn resolve_processes(proc_root: &Path) -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(proc_root) else {
        return out;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        // Only numeric directories are PIDs; "self", "net", etc. parse-fail and skip.
        let Some(pid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(name) = resolve_name(&path) {
            out.push(ProcEntry { pid, name });
        }
    }
    out
}

/// Whether `name` matches `pattern` under the same relaxed rule the Windows
/// custom `ProcessExists` sensor uses: equality (case-insensitive), with or
/// without a trailing `.exe`, or a case-insensitive substring. Centralized so
/// the Linux sensor gains `.exe`/long-name support instead of exact-comm only.
pub(crate) fn name_matches(name: &str, pattern: &str) -> bool {
    name_matches_exact(name, pattern)
        || name
            .to_ascii_lowercase()
            .contains(&pattern.to_ascii_lowercase())
}

/// Strict name match: equality (case-insensitive) with or without a trailing
/// `.exe`, but WITHOUT the substring rule [`name_matches`] adds. Used by the
/// close/kill path: signalling is destructive, so `close:vlc` must match only
/// `vlc`/`vlc.exe`, never `vlc-wrapper`.
pub(crate) fn name_matches_exact(name: &str, pattern: &str) -> bool {
    let name_stem = name.strip_suffix(".exe").unwrap_or(name);
    let pat_stem = pattern.strip_suffix(".exe").unwrap_or(pattern);
    name.eq_ignore_ascii_case(pattern) || name_stem.eq_ignore_ascii_case(pat_stem)
}

/// PIDs of live processes under `proc_root` whose resolved name matches
/// `pattern` EXACTLY (via [`name_matches_exact`] - no substring, since this
/// feeds a kill), owned by the current euid, excluding PID <= 1. The ownership
/// and low-pid guards mean a `close:`/`kill:` payload can never signal init or
/// another user's process. Empty on macOS (no `/proc`), so callers fall back to
/// `pkill -x`.
pub(crate) fn owned_pids_by_name(proc_root: &Path, pattern: &str) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid takes no args and has no side effects.
    let euid = unsafe { libc::geteuid() };
    resolve_processes(proc_root)
        .into_iter()
        .filter(|p| p.pid > 1 && name_matches_exact(&p.name, pattern))
        .filter(|p| {
            // The /proc/<pid> directory is owned by the process's real uid.
            std::fs::metadata(proc_root.join(p.pid.to_string()))
                .map(|m| m.uid())
                .ok()
                == Some(euid)
        })
        .map(|p| p.pid)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a fake /proc/<pid> dir with the given comm and optional cmdline.
    fn write_proc(root: &Path, pid: &str, comm: &str, cmdline: Option<&[&str]>) {
        let dir = root.join(pid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        if let Some(argv) = cmdline {
            let mut bytes = Vec::new();
            for a in argv {
                bytes.extend_from_slice(a.as_bytes());
                bytes.push(0);
            }
            fs::write(dir.join("cmdline"), bytes).unwrap();
        }
    }

    #[test]
    fn resolves_short_comm_directly() {
        let tmp = tempfile::tempdir().unwrap();
        write_proc(tmp.path(), "10", "steam", None);
        let procs = resolve_processes(tmp.path());
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 10);
        assert_eq!(procs[0].name, "steam");
    }

    #[test]
    fn resolves_truncated_comm_from_cmdline_basename() {
        let tmp = tempfile::tempdir().unwrap();
        // comm is exactly 15 chars (truncated); cmdline argv0 has the full path.
        write_proc(
            tmp.path(),
            "20",
            "MarvelRivals_Sh",
            Some(&["/games/MarvelRivals_Shipping.exe", "-nolog"]),
        );
        let procs = resolve_processes(tmp.path());
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].name, "MarvelRivals_Shipping.exe");
    }

    #[test]
    fn handles_backslash_argv0_and_skips_non_pid_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write_proc(
            tmp.path(),
            "30",
            "SomeLongCommName",
            Some(&["Z:\\game\\GTA5.exe"]),
        );
        // A non-numeric dir must be ignored.
        fs::create_dir_all(tmp.path().join("self")).unwrap();
        let procs = resolve_processes(tmp.path());
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].name, "GTA5.exe");
    }

    #[test]
    fn owned_pids_matches_by_name_and_excludes_low_pids() {
        let tmp = tempfile::tempdir().unwrap();
        // Temp dirs are owned by the current euid, so the ownership gate passes.
        write_proc(tmp.path(), "1234", "GTA5.exe", None);
        write_proc(tmp.path(), "1", "GTA5.exe", None); // pid 1 must be excluded
        write_proc(tmp.path(), "1235", "firefox", None); // non-match
        let pids = owned_pids_by_name(tmp.path(), "gta5");
        assert_eq!(pids, vec![1234]);
    }

    #[test]
    fn name_matches_exe_and_substring() {
        assert!(name_matches("GTA5.exe", "gta5"));
        assert!(name_matches("gta5", "GTA5.exe"));
        assert!(name_matches("MarvelRivals_Shipping.exe", "marvelrivals"));
        assert!(name_matches("steam", "steam"));
        assert!(!name_matches("firefox", "chrome"));
    }

    #[test]
    fn name_matches_exact_rejects_substring() {
        // Same eq / .exe-stem behavior as the relaxed matcher...
        assert!(name_matches_exact("GTA5.exe", "gta5"));
        assert!(name_matches_exact("gta5", "GTA5.exe"));
        assert!(name_matches_exact("steam", "steam"));
        // ...but a substring must NOT match (would over-kill).
        assert!(!name_matches_exact("vlc-wrapper", "vlc"));
        assert!(!name_matches_exact(
            "MarvelRivals_Shipping.exe",
            "marvelrivals"
        ));
    }

    #[test]
    fn owned_pids_uses_exact_match_not_substring() {
        let tmp = tempfile::tempdir().unwrap();
        write_proc(tmp.path(), "1000", "vlc", None);
        write_proc(tmp.path(), "1001", "vlc-wrapper", None); // must NOT be killed
        let pids = owned_pids_by_name(tmp.path(), "vlc");
        assert_eq!(pids, vec![1000]);
    }
}
