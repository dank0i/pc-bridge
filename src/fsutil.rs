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

    let tmp = path.with_extension("tmp");

    // Write + fsync the temp file.
    {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            if let Some(m) = mode {
                opts.mode(m);
            }
            opts.open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut file = {
            let _ = mode;
            std::fs::File::create(&tmp)?
        };

        file.write_all(bytes)?;
        file.sync_all()?;
    }

    // Atomic replace (same directory => same filesystem).
    std::fs::rename(&tmp, path)
}
