//! Persistent rotating file logger (zero external dependencies).
//!
//! pc-bridge runs as a background process with no attached console, so the
//! default `env_logger` stderr output is discarded. This module installs a
//! size-rotating file sink as `env_logger`'s pipe target, so every `log` macro
//! call is persisted to disk, while still mirroring output to stderr for
//! interactive `--setup` runs.
//!
//! env_logger serializes writes to its pipe target behind a `Mutex` (see
//! `env_logger`'s `writer::buffer`), so the writer below needs no internal
//! synchronization.
//!
//! Nothing sensitive is logged: existing call sites log only error messages,
//! fixed strings, and `host:port` - never credentials. As defense-in-depth the
//! log files are created owner-only (`0600` on Unix; `%LOCALAPPDATA%` is
//! per-user ACL-protected on Windows).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Maximum size of the active log file before it is rotated.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Number of rotated files to retain (`pc-bridge.log.1` ..= `pc-bridge.log.N`).
const MAX_BACKUPS: usize = 3;
/// Name of the active log file within the log directory.
const LOG_FILE_NAME: &str = "pc-bridge.log";

/// A size-rotating file writer that also mirrors output to stderr.
struct RotatingWriter {
    /// Path to the active log file.
    path: PathBuf,
    /// Handle to the active log file.
    file: File,
    /// Bytes written to the active file since it was last opened or rotated.
    written: u64,
    /// Size threshold that triggers rotation.
    max_bytes: u64,
}

impl RotatingWriter {
    /// Open (or create, appending to) the log file with the production limit.
    fn open(path: PathBuf) -> io::Result<Self> {
        Self::with_limit(path, MAX_LOG_BYTES)
    }

    /// Open with an explicit size limit. Separated out so tests can exercise
    /// rotation without writing megabytes.
    fn with_limit(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        let file = open_log(&path, false)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            file,
            written,
            max_bytes,
        })
    }

    /// Rotate `name.log` → `name.log.1`, shifting older backups up by one and
    /// dropping the oldest. Best-effort: a failed rename leaves existing logs
    /// intact and logging continues against the current file.
    fn rotate(&mut self) -> io::Result<()> {
        let _ = self.file.flush();

        // Drop the oldest backup, then shift the rest up by one slot.
        let _ = fs::remove_file(backup_path(&self.path, MAX_BACKUPS));
        for i in (1..MAX_BACKUPS).rev() {
            let from = backup_path(&self.path, i);
            if from.exists() {
                let _ = fs::rename(&from, backup_path(&self.path, i + 1));
            }
        }
        let _ = fs::rename(&self.path, backup_path(&self.path, 1));

        self.file = open_log(&self.path, true)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Mirror to stderr for interactive runs. In the windowed subsystem there
        // is no console and this is a silent no-op; it is never fatal.
        let _ = io::stderr().write_all(buf);

        if self.written.saturating_add(buf.len() as u64) > self.max_bytes {
            // A rotation failure must not lose the current record: fall through
            // and keep appending to the existing file.
            let _ = self.rotate();
        }

        let n = self.file.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Initialize logging: install the rotating file sink as `env_logger`'s target.
///
/// Falls back to plain stderr logging if the log file cannot be opened, so a
/// read-only or permission-denied log directory never prevents startup.
pub fn init() {
    let mut builder = env_logger::Builder::from_default_env();
    builder
        .filter_level(log::LevelFilter::Info)
        .format_target(false)
        .format_timestamp_secs();

    match log_file_path().and_then(RotatingWriter::open) {
        Ok(writer) => {
            let path = writer.path.clone();
            builder.target(env_logger::Target::Pipe(Box::new(writer)));
            builder.init();
            log::info!("Logging to {}", path.display());
        }
        Err(e) => {
            builder.init();
            log::warn!("File logging unavailable, using stderr only: {e}");
        }
    }
}

/// Resolve the log file path, creating the parent directory if needed.
fn log_file_path() -> io::Result<PathBuf> {
    let dir = log_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir.join(LOG_FILE_NAME))
}

/// Per-user log directory: `%LOCALAPPDATA%\pc-bridge` on Windows.
#[cfg(windows)]
fn log_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(base).join("pc-bridge");
    }
    std::env::temp_dir().join("pc-bridge")
}

/// Per-user log directory on Unix: `$XDG_STATE_HOME/pc-bridge`, else
/// `~/.local/state/pc-bridge`, else a temp directory.
#[cfg(unix)]
fn log_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(base).join("pc-bridge");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/pc-bridge");
    }
    std::env::temp_dir().join("pc-bridge")
}

/// Open the log file owner-only. `truncate` starts a fresh file (after
/// rotation); otherwise output is appended to any existing log.
fn open_log(path: &Path, truncate: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true);
    if truncate {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Build the path of the `n`th rotated backup: `pc-bridge.log` → `pc-bridge.log.1`.
fn backup_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_appends_index() {
        let p = Path::new("/var/log/pc-bridge.log");
        assert_eq!(backup_path(p, 1), PathBuf::from("/var/log/pc-bridge.log.1"));
        assert_eq!(backup_path(p, 3), PathBuf::from("/var/log/pc-bridge.log.3"));
    }

    #[test]
    fn rotates_when_exceeding_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pc-bridge.log");
        let mut w = RotatingWriter::with_limit(path.clone(), 64).unwrap();

        // 70 bytes total in 10-byte records crosses the 64-byte threshold.
        for _ in 0..7 {
            w.write_all(b"0123456789").unwrap();
        }
        w.flush().unwrap();

        assert!(path.exists(), "active log should exist");
        assert!(
            backup_path(&path, 1).exists(),
            "first backup should be created on rotation"
        );
    }

    #[test]
    fn retains_only_max_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pc-bridge.log");
        let mut w = RotatingWriter::with_limit(path.clone(), 10).unwrap();

        // Many oversized writes force repeated rotations.
        for _ in 0..20 {
            w.write_all(b"0123456789ABCDEF").unwrap();
        }
        w.flush().unwrap();

        assert!(
            !backup_path(&path, MAX_BACKUPS + 1).exists(),
            "backups beyond MAX_BACKUPS must be pruned"
        );
    }
}
