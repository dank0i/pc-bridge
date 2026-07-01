//! HWiNFO64 shared-memory sensor reader.
//!
//! HWiNFO publishes a shared-memory section named `Global\HWiNFO_SENS_SM2` when
//! "Shared Memory Support" is enabled in HWiNFO. This module opens the file
//! mapping, validates the header version, and parses sensor / reading tables.
//!
//! Parsing is done by explicit byte offsets (no `repr(C, packed)`) to avoid
//! alignment UB and to be tolerant of small layout changes between HWiNFO
//! versions. Every offset is bounds-checked against the mapped view; parse
//! errors return `anyhow::Result` instead of panicking.
//!
//! The producer (HWiNFO) writes a new snapshot periodically; consumers poll
//! the `pollTime` field at offset 16 to detect updates without doing a full
//! parse on every tick.

// On non-Windows targets the binary never instantiates `HwInfoClient` or
// invokes `parse_snapshot`, but the tests still exercise the pure parser. The
// module-level allow keeps clippy quiet on those targets without hiding real
// dead code on Windows (where everything is wired up via `sensors::hwinfo`).
#![cfg_attr(not(windows), allow(dead_code))]
#![allow(clippy::struct_field_names)]
#![allow(clippy::if_not_else)]

use anyhow::{Result, anyhow};

/// Cap on the mapped view size we'll trust from the header. HWiNFO's
/// shared-memory section is well under this on any real machine.
#[cfg(windows)]
const MAX_VIEW_SIZE: usize = 4 * 1024 * 1024;

/// Header layout offsets (see module docs).
///
/// HWiNFO declares its struct with `#pragma pack(1)` (no alignment padding),
/// so the `__int64 poll_time` field sits at offset 12 - NOT offset 16 as
/// natural 8-byte alignment would suggest. Every offset after `dwRevision`
/// is therefore 4 bytes lower than a naive C-struct layout would indicate.
/// This was verified empirically against HWiNFO Pro v8.46-5960; the
/// diagnostic showed `dwSizeOfSensorElement = 18` when read at offset 28,
/// which is actually `dwNumSensorElements` in the packed layout.
const OFF_SIGNATURE: usize = 0;
const OFF_VERSION: usize = 4;
#[allow(dead_code)]
const OFF_REVISION: usize = 8;
const OFF_POLLTIME: usize = 12;
const OFF_SENSOR_SECTION: usize = 20;
const OFF_SENSOR_ELEM_SIZE: usize = 24;
const OFF_NUM_SENSORS: usize = 28;
const OFF_READING_SECTION: usize = 32;
const OFF_READING_ELEM_SIZE: usize = 36;
const OFF_NUM_READINGS: usize = 40;
const HEADER_SIZE: usize = 44;

/// Sensor-element field offsets.
/// `SE_ID` (offset 0) and `SE_INSTANCE` (offset 4) are documented but unread
/// by the parser - sensors are referenced by positional index, so reading
/// these every snapshot would be ~10 wasted u32 loads. Kept only for the test
/// buffer builder, which writes the ID field into synthetic fixtures.
#[cfg(test)]
const SE_ID: usize = 0;
#[allow(dead_code)]
const SE_INSTANCE: usize = 4;
const SE_NAME_ORIG: usize = 8;
const SE_NAME_USER: usize = 136;
const SE_NAME_LEN: usize = 128;

/// Reading-element field offsets.
///
/// Same packing caveat as the header: HWiNFO uses `#pragma pack(1)`, so the
/// `double Value` field is NOT 8-byte aligned. It sits at offset 284 (right
/// after `szUnit[16]` ends at 268+16=284), not at offset 288 as natural
/// alignment would dictate.
const RE_TYPE: usize = 0;
const RE_SENSOR_INDEX: usize = 4;
#[allow(dead_code)]
const RE_READING_ID: usize = 8;
const RE_LABEL_ORIG: usize = 12;
const RE_LABEL_USER: usize = 140;
const RE_LABEL_LEN: usize = 128;
const RE_UNIT: usize = 268;
const RE_UNIT_LEN: usize = 16;
const RE_VALUE: usize = 284;
const RE_VALUE_MIN: usize = 292;
const RE_VALUE_MAX: usize = 300;
const RE_VALUE_AVG: usize = 308;

/// A single parsed reading from HWiNFO.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub sensor_name: String,
    pub label: String,
    pub unit: String,
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub reading_type: u32,
}

/// A full snapshot of HWiNFO's current shared-memory state.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// HWiNFO's `pollTime` (FILETIME) at the moment this snapshot was parsed.
    /// Currently informational - the sensor task uses
    /// `HwInfoClient::read_poll_time` for cheap pre-parse change detection.
    #[allow(dead_code)]
    pub poll_time: i64,
    pub readings: Vec<Reading>,
}

/// Parsed header values.
#[derive(Debug, Clone, Copy)]
struct Header {
    poll_time: i64,
    sensor_section: usize,
    sensor_elem_size: usize,
    num_sensors: usize,
    reading_section: usize,
    reading_elem_size: usize,
    num_readings: usize,
}

/// Trim a fixed-size NUL-padded UTF-8 field into an owned `String`.
fn trim_cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn read_u32(view: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| anyhow!("hwinfo: u32 offset overflow at {}", offset))?;
    let slice = view
        .get(offset..end)
        .ok_or_else(|| anyhow!("hwinfo: u32 read out of bounds at {}", offset))?;
    let arr: [u8; 4] = slice
        .try_into()
        .map_err(|_| anyhow!("hwinfo: u32 slice convert failed at {}", offset))?;
    Ok(u32::from_le_bytes(arr))
}

fn read_i64(view: &[u8], offset: usize) -> Result<i64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| anyhow!("hwinfo: i64 offset overflow at {}", offset))?;
    let slice = view
        .get(offset..end)
        .ok_or_else(|| anyhow!("hwinfo: i64 read out of bounds at {}", offset))?;
    let arr: [u8; 8] = slice
        .try_into()
        .map_err(|_| anyhow!("hwinfo: i64 slice convert failed at {}", offset))?;
    Ok(i64::from_le_bytes(arr))
}

fn read_f64(view: &[u8], offset: usize) -> Result<f64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| anyhow!("hwinfo: f64 offset overflow at {}", offset))?;
    let slice = view
        .get(offset..end)
        .ok_or_else(|| anyhow!("hwinfo: f64 read out of bounds at {}", offset))?;
    let arr: [u8; 8] = slice
        .try_into()
        .map_err(|_| anyhow!("hwinfo: f64 slice convert failed at {}", offset))?;
    Ok(f64::from_le_bytes(arr))
}

fn read_field(view: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| anyhow!("hwinfo: field offset overflow at {}", offset))?;
    view.get(offset..end)
        .ok_or_else(|| anyhow!("hwinfo: field read out of bounds at {}", offset))
}

/// Parse just the header. Validates `dwVersion ∈ {1, 2}` and returns the field
/// offsets needed to bound-check the rest of the parse.
fn parse_header(view: &[u8]) -> Result<Header> {
    if view.len() < HEADER_SIZE {
        return Err(anyhow!(
            "hwinfo: view too small for header ({} < {})",
            view.len(),
            HEADER_SIZE
        ));
    }

    let _signature = read_u32(view, OFF_SIGNATURE)?;
    let version = read_u32(view, OFF_VERSION)?;
    if version != 1 && version != 2 {
        return Err(anyhow!("hwinfo: unsupported header version {}", version));
    }

    let poll_time = read_i64(view, OFF_POLLTIME)?;
    let sensor_section = read_u32(view, OFF_SENSOR_SECTION)? as usize;
    let sensor_elem_size = read_u32(view, OFF_SENSOR_ELEM_SIZE)? as usize;
    let num_sensors = read_u32(view, OFF_NUM_SENSORS)? as usize;
    let reading_section = read_u32(view, OFF_READING_SECTION)? as usize;
    let reading_elem_size = read_u32(view, OFF_READING_ELEM_SIZE)? as usize;
    let num_readings = read_u32(view, OFF_NUM_READINGS)? as usize;

    if sensor_elem_size < SE_NAME_USER + SE_NAME_LEN {
        return Err(anyhow!(
            "hwinfo: sensor element too small ({} bytes)",
            sensor_elem_size
        ));
    }
    if reading_elem_size < RE_VALUE_AVG + 8 {
        return Err(anyhow!(
            "hwinfo: reading element too small ({} bytes)",
            reading_elem_size
        ));
    }

    Ok(Header {
        poll_time,
        sensor_section,
        sensor_elem_size,
        num_sensors,
        reading_section,
        reading_elem_size,
        num_readings,
    })
}

/// Parse all sensor names into a vector indexed by sensor element index.
fn parse_sensor_names(view: &[u8], header: &Header) -> Result<Vec<String>> {
    let total = header
        .sensor_elem_size
        .checked_mul(header.num_sensors)
        .and_then(|s| s.checked_add(header.sensor_section))
        .ok_or_else(|| anyhow!("hwinfo: sensor section size overflow"))?;
    if total > view.len() {
        return Err(anyhow!(
            "hwinfo: sensor section out of bounds ({} > {})",
            total,
            view.len()
        ));
    }

    let mut names = Vec::with_capacity(header.num_sensors);
    for i in 0..header.num_sensors {
        let base = header.sensor_section + i * header.sensor_elem_size;
        // Prefer the user-renamed name (HWiNFO lets users rename sensors in
        // its UI); fall back to the original when the user field is empty.
        // dwSensorID at offset 0 is intentionally not read - we key on
        // positional index, so skipping it avoids ~10 redundant reads per
        // snapshot.
        let orig = trim_cstr(read_field(view, base + SE_NAME_ORIG, SE_NAME_LEN)?);
        let user = trim_cstr(read_field(view, base + SE_NAME_USER, SE_NAME_LEN)?);
        let name = if user.is_empty() { orig } else { user };
        names.push(name);
    }
    Ok(names)
}

/// Parse all readings and join them with their owning sensor's name.
fn parse_readings(view: &[u8], header: &Header, sensor_names: &[String]) -> Result<Vec<Reading>> {
    let total = header
        .reading_elem_size
        .checked_mul(header.num_readings)
        .and_then(|s| s.checked_add(header.reading_section))
        .ok_or_else(|| anyhow!("hwinfo: reading section size overflow"))?;
    if total > view.len() {
        return Err(anyhow!(
            "hwinfo: reading section out of bounds ({} > {})",
            total,
            view.len()
        ));
    }

    let mut out = Vec::with_capacity(header.num_readings);
    for i in 0..header.num_readings {
        let base = header.reading_section + i * header.reading_elem_size;

        let reading_type = read_u32(view, base + RE_TYPE)?;
        let sensor_index = read_u32(view, base + RE_SENSOR_INDEX)? as usize;
        let label_orig = trim_cstr(read_field(view, base + RE_LABEL_ORIG, RE_LABEL_LEN)?);
        let label_user = trim_cstr(read_field(view, base + RE_LABEL_USER, RE_LABEL_LEN)?);
        let label = if !label_user.is_empty() {
            label_user
        } else {
            label_orig
        };
        let unit = trim_cstr(read_field(view, base + RE_UNIT, RE_UNIT_LEN)?);
        let value = read_f64(view, base + RE_VALUE)?;
        let min = read_f64(view, base + RE_VALUE_MIN)?;
        let max = read_f64(view, base + RE_VALUE_MAX)?;
        let avg = read_f64(view, base + RE_VALUE_AVG)?;

        let sensor_name = sensor_names.get(sensor_index).cloned().unwrap_or_default();

        out.push(Reading {
            sensor_name,
            label,
            unit,
            value,
            min,
            max,
            avg,
            reading_type,
        });
    }
    Ok(out)
}

/// Parse a complete `Snapshot` from a mapped view.
pub fn parse_snapshot(view: &[u8]) -> Result<Snapshot> {
    let header = parse_header(view)?;
    let sensor_names = parse_sensor_names(view, &header)?;
    let readings = parse_readings(view, &header, &sensor_names)?;
    Ok(Snapshot {
        poll_time: header.poll_time,
        readings,
    })
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::{HEADER_SIZE, MAX_VIEW_SIZE, OFF_POLLTIME, parse_snapshot, read_i64, read_u32};
    use super::{OFF_NUM_READINGS, OFF_READING_ELEM_SIZE, OFF_READING_SECTION};
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
    };
    use windows::core::w;

    /// RAII wrapper around an HWiNFO shared-memory mapping.
    ///
    /// The mapped section is bounded by the underlying file mapping HWiNFO
    /// created at startup, but its *useful* size (number of sensors/readings)
    /// can grow at runtime as HWiNFO enumerates additional hardware. To avoid
    /// stale-slice bounds-check failures after such growth, `as_slice()`
    /// re-probes the header on every call and returns a slice sized to match
    /// the *current* header - never the size captured at `open()` time.
    pub struct HwInfoClient {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        /// Actual mapped region size (from VirtualQuery at open). The hard upper
        /// bound on any slice we expose, so a corrupt header can't slice past it.
        region_size: usize,
    }

    // SAFETY: HwInfoClient owns a kernel handle and a mapped view; both are
    // valid across threads as long as we don't free them concurrently. The
    // surrounding logic uses the client from a single tokio task only - but
    // tokio may move tasks across threads, so we mark Send. The raw pointer in
    // `view.Value` is not aliased while the client lives.
    unsafe impl Send for HwInfoClient {}

    impl HwInfoClient {
        pub fn open() -> Option<Self> {
            // SAFETY: We pass a static wide string for the mapping name. The
            // returned HANDLE is validated below; on failure we return None.
            let handle_res =
                unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, w!("Global\\HWiNFO_SENS_SM2")) };
            let handle = match handle_res {
                Ok(h) if !h.is_invalid() => h,
                _ => return None,
            };

            // SAFETY: `handle` is a valid file-mapping handle. Size 0 means
            // "map the entire underlying section" - HWiNFO created it with a
            // fixed bounded size. If MapViewOfFile fails, .Value is null and
            // we close the handle and bail.
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
            if view.Value.is_null() {
                // SAFETY: handle was successfully opened above.
                let _ = unsafe { CloseHandle(handle) };
                return None;
            }

            // Capture the real mapped region size so a corrupt/torn header can
            // never make us slice past the actual mapping (access violation).
            // SAFETY: view.Value is a valid mapped pointer; VirtualQuery only
            // reads process memory-map metadata for that address.
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            let region_size = unsafe {
                if VirtualQuery(
                    Some(view.Value.cast_const()),
                    &raw mut mbi,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                ) != 0
                {
                    mbi.RegionSize
                } else {
                    // Conservative fallback if the query fails.
                    MAX_VIEW_SIZE
                }
            };

            Some(Self {
                handle,
                view,
                region_size,
            })
        }

        /// Re-probe the header to compute the currently-required view size in
        /// bytes. Returns at least `HEADER_SIZE` and at most `MAX_VIEW_SIZE`.
        ///
        /// This is called on every `as_slice()` so that if HWiNFO grew its
        /// sensor or reading table after we opened the mapping (e.g. it
        /// enumerated additional hardware), our slice grows to match instead
        /// of clipping the new region away.
        fn current_size(&self) -> usize {
            // Hard cap: never exceed the actual mapped region, so a torn/corrupt
            // header advertising a huge `needed` can't make us slice out of bounds.
            let cap = MAX_VIEW_SIZE.min(self.region_size);
            let head_len = HEADER_SIZE.min(self.region_size);
            // SAFETY: `head_len <= region_size`, so this stays within the mapping.
            let head_slice =
                unsafe { std::slice::from_raw_parts(self.view.Value.cast::<u8>(), head_len) };
            let reading_section = read_u32(head_slice, OFF_READING_SECTION)
                .map(|v| v as usize)
                .unwrap_or(0);
            let reading_elem_size = read_u32(head_slice, OFF_READING_ELEM_SIZE)
                .map(|v| v as usize)
                .unwrap_or(0);
            let num_readings = read_u32(head_slice, OFF_NUM_READINGS)
                .map(|v| v as usize)
                .unwrap_or(0);
            let needed = reading_section
                .checked_add(reading_elem_size.saturating_mul(num_readings))
                .unwrap_or(cap);
            needed.clamp(HEADER_SIZE.min(self.region_size), cap)
        }

        /// Borrow the mapped view as a byte slice sized to the current header.
        fn as_slice(&self) -> &[u8] {
            let size = self.current_size();
            // SAFETY: `self.view.Value` is a valid mapped pointer; the
            // underlying file mapping created by HWiNFO is bounded in length,
            // and `current_size()` clamps the slice we expose to at most
            // MAX_VIEW_SIZE. HWiNFO's header always advertises a `needed`
            // value within the kernel-owned mapping it provisioned. We never
            // hand out a mutable reference; concurrent producer writes are
            // atomic per record - a torn read yields stale numbers, never UB.
            unsafe { std::slice::from_raw_parts(self.view.Value.cast::<u8>(), size) }
        }

        /// Current view size in bytes (re-probed from the live header). Used
        /// by the diagnostic publisher.
        pub fn view_size_bytes(&self) -> usize {
            self.current_size()
        }

        /// Read just the 8-byte `pollTime` field. Cheap lazy-poll primitive.
        /// Returns `None` if the view is somehow too small (should never
        /// happen - `as_slice()` always returns at least `HEADER_SIZE` bytes).
        pub fn read_poll_time(&self) -> Option<i64> {
            let slice = self.as_slice();
            if slice.len() < OFF_POLLTIME + 8 {
                return None;
            }
            read_i64(slice, OFF_POLLTIME).ok()
        }

        /// Parse a full snapshot from the current view.
        pub fn snapshot(&self) -> Result<super::Snapshot> {
            parse_snapshot(self.as_slice()).with_context(|| "hwinfo: parse_snapshot failed")
        }
    }

    impl Drop for HwInfoClient {
        fn drop(&mut self) {
            if !self.view.Value.is_null() {
                // SAFETY: `self.view` was produced by MapViewOfFile and not yet
                // unmapped (we only run Drop once).
                let _ = unsafe { UnmapViewOfFile(self.view) };
            }
            if !self.handle.is_invalid() {
                // SAFETY: `self.handle` came from OpenFileMappingW and has not
                // been closed yet.
                let _ = unsafe { CloseHandle(self.handle) };
            }
        }
    }
}

#[cfg(windows)]
pub use win::HwInfoClient;

// ---------------------------------------------------------------------------
// Non-Windows stub: lets the rest of the crate compile cleanly. The Windows
// sensor task is gated behind `#[cfg(windows)]` and never instantiates this.
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
pub struct HwInfoClient {
    _private: (),
}

#[cfg(not(windows))]
impl HwInfoClient {
    pub fn open() -> Option<Self> {
        None
    }
    pub fn read_poll_time(&self) -> Option<i64> {
        None
    }
    pub fn view_size_bytes(&self) -> usize {
        0
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        Err(anyhow!("hwinfo: shared memory is Windows-only"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// One reading row in the test buffer.
    /// Tuple shape: (sensor_id, sensor_index, label, unit, value, min, max, avg)
    type TestReading<'a> = (u32, usize, &'a str, &'a str, f64, f64, f64, f64);

    /// Build a minimal HWiNFO-format byte buffer for testing.
    fn build_test_buffer(version: u32, sensors: &[&str], readings: &[TestReading]) -> Vec<u8> {
        // Packed layout: sensor element is dwSensorID(4) + dwSensorInst(4) +
        // szSensorNameOrig[128] + szSensorNameUser[128] = 264 bytes (no padding).
        // Reading element is RE_VALUE_AVG(308) + 8 = 316 bytes (packed; Value
        // sits at offset 284 with no alignment padding for the f64).
        let sensor_elem_size = 264usize;
        let reading_elem_size = 316usize;
        let sensor_section = HEADER_SIZE;
        let reading_section = sensor_section + sensor_elem_size * sensors.len();
        let total = reading_section + reading_elem_size * readings.len();

        let mut buf = vec![0u8; total];

        // Header
        buf[OFF_SIGNATURE..OFF_SIGNATURE + 4].copy_from_slice(&0x5349_5748u32.to_le_bytes());
        buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&version.to_le_bytes());
        buf[OFF_REVISION..OFF_REVISION + 4].copy_from_slice(&1u32.to_le_bytes());
        buf[OFF_POLLTIME..OFF_POLLTIME + 8].copy_from_slice(&123_456_789i64.to_le_bytes());
        buf[OFF_SENSOR_SECTION..OFF_SENSOR_SECTION + 4]
            .copy_from_slice(&(sensor_section as u32).to_le_bytes());
        buf[OFF_SENSOR_ELEM_SIZE..OFF_SENSOR_ELEM_SIZE + 4]
            .copy_from_slice(&(sensor_elem_size as u32).to_le_bytes());
        buf[OFF_NUM_SENSORS..OFF_NUM_SENSORS + 4]
            .copy_from_slice(&(sensors.len() as u32).to_le_bytes());
        buf[OFF_READING_SECTION..OFF_READING_SECTION + 4]
            .copy_from_slice(&(reading_section as u32).to_le_bytes());
        buf[OFF_READING_ELEM_SIZE..OFF_READING_ELEM_SIZE + 4]
            .copy_from_slice(&(reading_elem_size as u32).to_le_bytes());
        buf[OFF_NUM_READINGS..OFF_NUM_READINGS + 4]
            .copy_from_slice(&(readings.len() as u32).to_le_bytes());

        // Sensor elements
        for (i, name) in sensors.iter().enumerate() {
            let base = sensor_section + i * sensor_elem_size;
            buf[base + SE_ID..base + SE_ID + 4].copy_from_slice(&(i as u32).to_le_bytes());
            let bytes = name.as_bytes();
            let len = bytes.len().min(SE_NAME_LEN - 1);
            buf[base + SE_NAME_ORIG..base + SE_NAME_ORIG + len].copy_from_slice(&bytes[..len]);
        }

        // Reading elements
        for (i, &(rtype, sidx, label, unit, value, min, max, avg)) in readings.iter().enumerate() {
            let base = reading_section + i * reading_elem_size;
            buf[base + RE_TYPE..base + RE_TYPE + 4].copy_from_slice(&rtype.to_le_bytes());
            buf[base + RE_SENSOR_INDEX..base + RE_SENSOR_INDEX + 4]
                .copy_from_slice(&(sidx as u32).to_le_bytes());

            let lbytes = label.as_bytes();
            let llen = lbytes.len().min(RE_LABEL_LEN - 1);
            buf[base + RE_LABEL_ORIG..base + RE_LABEL_ORIG + llen].copy_from_slice(&lbytes[..llen]);

            let ubytes = unit.as_bytes();
            let ulen = ubytes.len().min(RE_UNIT_LEN - 1);
            buf[base + RE_UNIT..base + RE_UNIT + ulen].copy_from_slice(&ubytes[..ulen]);

            buf[base + RE_VALUE..base + RE_VALUE + 8].copy_from_slice(&value.to_le_bytes());
            buf[base + RE_VALUE_MIN..base + RE_VALUE_MIN + 8].copy_from_slice(&min.to_le_bytes());
            buf[base + RE_VALUE_MAX..base + RE_VALUE_MAX + 8].copy_from_slice(&max.to_le_bytes());
            buf[base + RE_VALUE_AVG..base + RE_VALUE_AVG + 8].copy_from_slice(&avg.to_le_bytes());
        }

        buf
    }

    #[test]
    fn test_parse_header_extracts_fields() {
        let buf = build_test_buffer(2, &["CPU [#0]: AMD 9800X3D"], &[]);
        let header = parse_header(&buf).expect("header");
        assert_eq!(header.poll_time, 123_456_789);
        assert_eq!(header.num_sensors, 1);
        assert_eq!(header.num_readings, 0);
        assert_eq!(header.sensor_elem_size, 264);
        assert_eq!(header.reading_elem_size, 316);
    }

    #[test]
    fn test_parse_header_rejects_unknown_version() {
        let buf = build_test_buffer(3, &[], &[]);
        let err = parse_header(&buf).expect_err("should reject");
        assert!(err.to_string().contains("unsupported header version"));
    }

    #[test]
    fn test_parse_header_rejects_too_small_view() {
        let buf = vec![0u8; 32];
        let err = parse_header(&buf).expect_err("should reject");
        assert!(err.to_string().contains("view too small"));
    }

    #[test]
    fn test_parse_sensor_and_reading_join() {
        let buf = build_test_buffer(
            2,
            &["CPU [#0]: AMD 9800X3D", "GPU [#0]: NVIDIA GeForce RTX 4090"],
            &[
                (1, 0, "CPU (Tctl/Tdie)", "°C", 65.5, 30.0, 90.0, 50.0),
                (5, 0, "CPU Package Power", "W", 95.0, 10.0, 142.0, 80.0),
                (1, 1, "GPU Temperature", "°C", 55.0, 30.0, 80.0, 60.0),
                (
                    6,
                    1,
                    "GPU Memory Clock",
                    "MHz",
                    10501.0,
                    0.0,
                    10501.0,
                    9000.0,
                ),
            ],
        );

        let snap = parse_snapshot(&buf).expect("snapshot");
        assert_eq!(snap.poll_time, 123_456_789);
        assert_eq!(snap.readings.len(), 4);

        let cpu_temp = &snap.readings[0];
        assert_eq!(cpu_temp.sensor_name, "CPU [#0]: AMD 9800X3D");
        assert_eq!(cpu_temp.label, "CPU (Tctl/Tdie)");
        assert_eq!(cpu_temp.unit, "°C");
        assert!((cpu_temp.value - 65.5).abs() < f64::EPSILON);
        assert!((cpu_temp.max - 90.0).abs() < f64::EPSILON);
        assert_eq!(cpu_temp.reading_type, 1);

        let gpu_mem_clock = &snap.readings[3];
        assert_eq!(
            gpu_mem_clock.sensor_name,
            "GPU [#0]: NVIDIA GeForce RTX 4090"
        );
        assert_eq!(gpu_mem_clock.label, "GPU Memory Clock");
        assert_eq!(gpu_mem_clock.unit, "MHz");
    }

    #[test]
    fn test_trim_cstr_truncates_at_nul() {
        let mut field = vec![0u8; 32];
        field[..5].copy_from_slice(b"Hello");
        assert_eq!(trim_cstr(&field), "Hello");
    }

    #[test]
    fn test_trim_cstr_handles_no_nul() {
        let field = b"abcdef".to_vec();
        assert_eq!(trim_cstr(&field), "abcdef");
    }

    #[test]
    fn test_trim_cstr_handles_empty() {
        let field = vec![0u8; 16];
        assert_eq!(trim_cstr(&field), "");
    }

    #[test]
    fn test_parse_snapshot_rejects_reading_section_out_of_bounds() {
        // Build a valid buffer then corrupt num_readings to claim more readings
        // than the buffer holds. Parser should return Err, not panic.
        let mut buf =
            build_test_buffer(2, &["CPU"], &[(1, 0, "Tctl", "°C", 50.0, 0.0, 100.0, 50.0)]);
        buf[OFF_NUM_READINGS..OFF_NUM_READINGS + 4].copy_from_slice(&9999u32.to_le_bytes());
        let err = parse_snapshot(&buf).expect_err("should reject");
        let msg = err.to_string();
        assert!(msg.contains("out of bounds"), "msg was: {}", msg);
    }

    #[test]
    fn test_parse_snapshot_rejects_sensor_section_out_of_bounds() {
        let mut buf = build_test_buffer(2, &["CPU"], &[]);
        buf[OFF_NUM_SENSORS..OFF_NUM_SENSORS + 4].copy_from_slice(&9999u32.to_le_bytes());
        let err = parse_snapshot(&buf).expect_err("should reject");
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn test_parse_header_rejects_too_small_sensor_elem() {
        let mut buf = build_test_buffer(2, &[], &[]);
        // Set sensor_elem_size to a tiny value
        buf[OFF_SENSOR_ELEM_SIZE..OFF_SENSOR_ELEM_SIZE + 4].copy_from_slice(&8u32.to_le_bytes());
        let err = parse_header(&buf).expect_err("should reject");
        assert!(err.to_string().contains("sensor element too small"));
    }

    #[test]
    fn test_parse_supports_version_1() {
        let buf = build_test_buffer(1, &["CPU"], &[]);
        let header = parse_header(&buf).expect("v1 header");
        assert_eq!(header.num_sensors, 1);
    }
}
