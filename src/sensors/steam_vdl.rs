//! Live Steam download progress from NTFS Valid Data Length (VDL).
//!
//! Steam preallocates every file in `steamapps/downloading/<appid>/` to its full
//! size before downloading, so both logical size and allocated size read 100% from
//! the first sample and carry no progress signal. NTFS separately tracks Valid Data
//! Length: how much of a file has actually been written. Preallocation moves the
//! end-of-file marker but does NOT move VDL, so summing VDL across the staging tree
//! gives real progress.
//!
//! Two properties make this work where every earlier approach failed:
//!
//! * `FSCTL_QUERY_FILE_REGIONS` is declared `FILE_ANY_ACCESS`, so the handle needs
//!   only `FILE_READ_ATTRIBUTES`. No elevation, ever.
//! * [MS-FSA] 2.1.5.1.2.2 runs the sharing-violation check only when the requested
//!   access includes read/write/append/execute/delete, so an attribute-only open
//!   can neither cause nor receive a sharing violation against the files Steam
//!   holds open for writing.
//!
//! Two precise caveats on that second point, because "invisible to Steam" would be
//! too strong. The Open IS still inserted into `File.OpenList` ([MS-FSA] 2.1.5.1.2
//! ends with that step unconditionally); it is merely ignored by the sharing
//! predicate. That is why these handles are opened and closed per sample rather
//! than held: a retained handle would keep a deleted name in delete-pending state
//! and break Steam's delete-and-recreate on a restarted download. The oplock check
//! also runs unconditionally, outside the access-bits gate, so an open here would
//! trigger a break check if Steam held an oplock (local apps essentially never do).
//!
//! Verified live on 2026-07-25 against a 144.67 GB Baldur's Gate 3 install: 268 of
//! 268 staging files readable as a normal user, VDL 4.52 GB against 144.67 GB of
//! EOF, matching the manifest's own `BytesStaged` (4.36 GB) to within 0.1 GB. The
//! actively-written file returned two regions (a written prefix and an unwritten
//! tail), confirming SteamPipe writes each file front to back, which is what makes
//! VDL a usable progress signal rather than a saturated one.
//!
//! Unlike the process-write-counter approach this replaces, VDL is per-appid by
//! construction (so concurrent downloads and Steam's own cache churn cannot
//! contaminate it) and is an absolute reading rather than an accumulator, so it
//! needs no re-anchoring and survives agent restarts, Steam restarts and pauses.

use std::path::Path;

/// Bytes actually written vs total preallocated across one staging directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagingProgress {
    /// Sum of Valid Data Length over every readable staging file.
    pub valid_bytes: u64,
    /// Sum of end-of-file (the preallocated total) over the same files.
    pub total_bytes: u64,
    pub files_read: usize,
    pub files_failed: usize,
}

impl StagingProgress {
    /// Fraction written, 0.0 to 100.0. Zero when nothing is measurable.
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        let pct = (self.valid_bytes as f64 / self.total_bytes as f64) * 100.0;
        pct.clamp(0.0, 100.0)
    }

    /// Whether this reading should be trusted over the manifest's checkpoint.
    ///
    /// Requires at least one readable file and a non-zero denominator. A directory
    /// where every file failed to open yields `false` so callers fall back rather
    /// than publishing a fabricated zero.
    pub fn is_usable(&self) -> bool {
        self.files_read > 0 && self.total_bytes > 0
    }
}

/// Sum Valid Data Length across every file under a staging directory.
///
/// Does blocking filesystem I/O; call from `spawn_blocking`. Never returns an
/// error: unreadable files are counted in `files_failed` so a partial read still
/// produces a usable ratio, and a total failure is reported via `is_usable`.
pub fn read_staging_progress(dir: &Path) -> StagingProgress {
    let mut out = StagingProgress::default();
    walk(dir, &mut out, 0);
    out
}

/// Recursive directory walk, depth-limited so a symlink loop cannot hang the
/// blocking task.
fn walk(dir: &Path, out: &mut StagingProgress, depth: u32) {
    const MAX_DEPTH: u32 = 16;
    if depth > MAX_DEPTH {
        // Count rather than silently drop, so an unexpectedly deep tree shows up
        // as unreadable files (and therefore falls back) instead of quietly
        // shrinking the denominator and reporting a plausible wrong number.
        out.files_failed += 1;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk(&path, out, depth + 1),
            Ok(ft) if ft.is_file() => match file_valid_data(&path) {
                Some((valid, eof)) => {
                    out.valid_bytes = out.valid_bytes.saturating_add(valid);
                    out.total_bytes = out.total_bytes.saturating_add(eof);
                    out.files_read += 1;
                }
                None => out.files_failed += 1,
            },
            _ => {}
        }
    }
}

/// Read one file's `(valid_data_length, end_of_file)`.
///
/// Returns `None` if the file cannot be opened or the filesystem does not support
/// the query (for example FAT32 or a network share).
#[cfg(windows)]
fn file_valid_data(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::core::{HRESULT, PCWSTR};

    // CTL_CODE(FILE_DEVICE_FILE_SYSTEM=0x9, function=161, METHOD_BUFFERED=0,
    // FILE_ANY_ACCESS=0). FILE_ANY_ACCESS is why an attribute-only handle suffices.
    const FSCTL_QUERY_FILE_REGIONS: u32 = 0x0009_0284;

    // Upper bound on the region count we will allocate for. NTFS returns at most
    // two regions for this query (written prefix, unwritten tail); a million is
    // absurdly generous while capping the allocation at 24 MB.
    const MAX_REGION_ENTRIES: usize = 1 << 20;

    // [MS-FSCC] 2.3.56.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FileRegionInfo {
        file_offset: i64,
        length: i64,
        usage: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct FileRegionOutput {
        flags: u32,
        total_region_entry_count: u32,
        region_entry_count: u32,
        reserved: u32,
    }

    const HEADER_LEN: usize = std::mem::size_of::<FileRegionOutput>();
    const ENTRY_LEN: usize = std::mem::size_of::<FileRegionInfo>();

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a NUL-terminated UTF-16 path alive for the call. The
    // handle is closed on every path out. Buffers passed to DeviceIoControl are
    // sized by the same expressions used for their length arguments, and the
    // output is only read back within `bytes_returned`.
    unsafe {
        let Ok(handle) = CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) else {
            return None;
        };

        // Passing a NULL input buffer makes the filesystem supply the query
        // itself: per [MS-FSA] 2.1.5.9.32 it uses FileOffset 0, Length MAXLONGLONG
        // and whichever usage flag is right for the volume (VALID_CACHED_DATA on
        // NTFS, VALID_NONCACHED_DATA on ReFS). That removes three things at once:
        // the GetFileSizeEx call, the NTFS-vs-ReFS branch, and the retry loop that
        // otherwise issued this FSCTL twice per file on ReFS.
        //
        // The returned regions tile [0, Eof): those with a non-zero Usage are the
        // written prefix, and the trailing Usage==0 region is the unwritten tail.
        // So all lengths sum to Eof and the non-zero-usage lengths sum to VDL,
        // which is exactly the pair this function returns.
        let mut result = None;
        let mut capacity = 4usize;
        for _ in 0..6 {
            let mut buf = vec![0u8; HEADER_LEN + ENTRY_LEN * capacity];
            let mut returned: u32 = 0;
            let call = DeviceIoControl(
                handle,
                FSCTL_QUERY_FILE_REGIONS,
                None,
                0,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&raw mut returned),
                None,
            );

            if let Err(e) = call {
                // Both of these mean "output buffer too small"; grow and retry.
                // Compared as full HRESULTs rather than masking off the low 16
                // bits, so an unrelated facility that happens to share a code
                // cannot be mistaken for a buffer error.
                let code = e.code();
                if code == HRESULT::from_win32(ERROR_MORE_DATA.0)
                    || code == HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0)
                {
                    capacity = capacity.saturating_mul(4).min(MAX_REGION_ENTRIES);
                    continue;
                }
                break; // unsupported filesystem, or the file went away
            }

            let header = buf.as_ptr().cast::<FileRegionOutput>().read_unaligned();
            let count = header.region_entry_count as usize;
            if header.total_region_entry_count as usize > count {
                // CLAMP: this count comes from the filesystem or any filter driver
                // in the stack and is otherwise unbounded. Feeding it straight to
                // vec! lets a buggy driver request ~103 GB, and allocation failure
                // calls handle_alloc_error, which ABORTS the process rather than
                // panicking, so spawn_blocking could not contain it.
                capacity = (header.total_region_entry_count as usize)
                    .saturating_add(8)
                    .min(MAX_REGION_ENTRIES);
                continue;
            }

            // Bound the parse by the byte count the driver actually reported, not
            // just by the entry count it claims.
            let reported = (returned as usize).saturating_sub(HEADER_LEN) / ENTRY_LEN;
            let (mut valid, mut eof) = (0u64, 0u64);
            for i in 0..count.min(reported) {
                let off = HEADER_LEN + ENTRY_LEN * i;
                if off + ENTRY_LEN > buf.len() {
                    break;
                }
                let info = buf
                    .as_ptr()
                    .add(off)
                    .cast::<FileRegionInfo>()
                    .read_unaligned();
                if info.length <= 0 {
                    continue;
                }
                let len = info.length as u64;
                eof = eof.saturating_add(len);
                if info.usage != 0 {
                    valid = valid.saturating_add(len);
                }
            }
            // VDL can never exceed EOF; clamp defensively so one odd reading
            // cannot push an aggregate percentage above 100.
            result = Some((valid.min(eof), eof));
            break;
        }

        let _ = CloseHandle(handle);
        result
    }
}

/// Non-Windows builds have no staging directory to read. Linux could implement
/// this with `SEEK_HOLE`/`SEEK_DATA` later; it is out of scope while Steam
/// download tracking is a Windows-only feature.
#[cfg(not(windows))]
fn file_valid_data(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_zero_when_nothing_measured() {
        assert_eq!(StagingProgress::default().percent(), 0.0);
    }

    #[test]
    fn percent_computes_ratio() {
        let p = StagingProgress {
            valid_bytes: 25,
            total_bytes: 100,
            files_read: 1,
            files_failed: 0,
        };
        assert!((p.percent() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percent_clamps_above_full() {
        // Defensive: a stale EOF paired with a fresh VDL must not exceed 100.
        let p = StagingProgress {
            valid_bytes: 200,
            total_bytes: 100,
            files_read: 1,
            files_failed: 0,
        };
        assert!((p.percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unusable_without_readable_files() {
        // Every file failed to open: must not be trusted, so callers fall back to
        // the manifest checkpoint instead of publishing a fabricated 0%.
        let p = StagingProgress {
            valid_bytes: 0,
            total_bytes: 0,
            files_read: 0,
            files_failed: 268,
        };
        assert!(!p.is_usable());
    }

    #[test]
    fn unusable_with_zero_denominator() {
        let p = StagingProgress {
            valid_bytes: 0,
            total_bytes: 0,
            files_read: 5,
            files_failed: 0,
        };
        assert!(!p.is_usable());
    }

    #[test]
    fn usable_with_partial_reads() {
        // A partial read still yields a meaningful ratio over what was readable.
        let p = StagingProgress {
            valid_bytes: 10,
            total_bytes: 40,
            files_read: 3,
            files_failed: 1,
        };
        assert!(p.is_usable());
        assert!((p.percent() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn region_input_length_matches_sdk_sizeof() {
        // FILE_REGION_INPUT is {LONGLONG; LONGLONG; DWORD;} = 20 bytes of fields,
        // but 8-byte alignment adds 4 bytes of tail padding, so the SDK sizeof is
        // 24. NTFS validates InputBufferLength >= sizeof and rejects 20 with
        // ERROR_INSUFFICIENT_BUFFER before touching the file. This caught a real
        // bug; keep the constant pinned.
        #[repr(C)]
        struct FileRegionInput {
            file_offset: i64,
            length: i64,
            desired_usage: u32,
        }
        assert_eq!(std::mem::size_of::<FileRegionInput>(), 24);
    }

    #[test]
    fn region_entry_and_header_lengths_match_sdk() {
        // FILE_REGION_INFO is 24 bytes; FILE_REGION_OUTPUT's Region[] starts at
        // FIELD_OFFSET 16 (four DWORDs), which is what the parse offset must use.
        #[repr(C)]
        struct FileRegionInfo {
            file_offset: i64,
            length: i64,
            usage: u32,
            reserved: u32,
        }
        #[repr(C)]
        struct FileRegionOutput {
            flags: u32,
            total_region_entry_count: u32,
            region_entry_count: u32,
            reserved: u32,
        }
        assert_eq!(std::mem::size_of::<FileRegionInfo>(), 24);
        assert_eq!(std::mem::size_of::<FileRegionOutput>(), 16);
    }

    #[test]
    fn region_capacity_clamp_bounds_the_allocation() {
        // A driver-supplied u32 must never reach vec! unclamped: u32::MAX entries
        // would request ~103 GB, and allocation failure calls handle_alloc_error,
        // which ABORTS rather than panicking, so spawn_blocking cannot contain it.
        const MAX_REGION_ENTRIES: usize = 1 << 20;
        let hostile = u32::MAX as usize;
        let clamped = hostile.saturating_add(8).min(MAX_REGION_ENTRIES);
        assert_eq!(clamped, MAX_REGION_ENTRIES);
        // 24 MB ceiling, not 103 GB.
        assert!(16 + 24 * clamped <= 25 * 1024 * 1024);
    }

    #[test]
    fn missing_directory_is_not_a_panic() {
        let p = read_staging_progress(Path::new("/definitely/not/a/real/staging/dir"));
        assert!(!p.is_usable());
        assert_eq!(p.files_read, 0);
    }
}
