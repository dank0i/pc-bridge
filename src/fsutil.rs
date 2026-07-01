//! Small filesystem helpers.

use std::io;
use std::path::Path;

/// Write `bytes` to `path` atomically: write a sibling temp file, fsync it, then
/// rename it over the destination. A crash mid-write leaves either the old file
/// or the complete new file, never a truncated one (which would corrupt the
/// config or empty the credential, and the hot-reload watcher reacts to partial
/// writes).
///
/// On Unix, `mode` sets the temp file's permissions *before* any bytes are
/// written, so there is no world-readable window (relevant for the credential
/// file, which may hold plaintext on non-Windows).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let pid = std::process::id();

    // Unique per-process temp name + O_EXCL create (create_new). Two concurrent
    // writers - e.g. the agent's Steam-refresh save racing a separate settings
    // (`--ui`) process's Save - must NOT share the same temp inode, or their
    // interleaved writes commit a corrupt `userConfig.json` that won't parse. A
    // leftover temp from a prior crash (same pid+counter after pid reuse) is
    // skipped by advancing the counter.
    let (mut file, tmp) = loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{pid}.{n}"));

        #[cfg(unix)]
        let opened = {
            use std::os::unix::fs::OpenOptionsExt;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            if let Some(m) = mode {
                opts.mode(m);
            }
            opts.open(&tmp)
        };
        #[cfg(not(unix))]
        let opened = {
            let _ = mode;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
        };

        match opened {
            Ok(f) => break (f, tmp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };

    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    // Atomic replace (same directory => same filesystem). Clean up our temp on
    // failure so a rename error doesn't strand it.
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            // fsync the parent directory so the rename entry itself is durable:
            // without this, a crash/power-loss right after the rename can revert to
            // the prior file on some filesystems (never a torn file, just the older
            // complete one). Best-effort, Unix only (Windows has no dir-fsync).
            #[cfg(unix)]
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
                && let Ok(d) = std::fs::File::open(dir)
            {
                let _ = d.sync_all();
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}
