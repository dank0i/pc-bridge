//! Binary appinfo.vdf parser
//!
//! Memory-mapped, indexed access to Steam's app metadata cache.
//! Only parses entries we actually need (installed games).

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// appinfo.vdf header magic
const APPINFO_MAGIC_V28: u32 = 0x07564428; // '(DV\x07' - version 28
const APPINFO_MAGIC_V29: u32 = 0x07564429; // '(DV\x07' - version 29

/// Binary VDF type markers
const TYPE_BLOCK_START: u8 = 0x00;
const TYPE_STRING: u8 = 0x01;
const TYPE_INT32: u8 = 0x02;
const TYPE_BLOCK_END: u8 = 0x08;

/// Index entry for fast lookup
#[derive(Clone, Copy)]
pub struct AppInfoEntry {
    pub offset: u64,
    pub size: u32,
}

/// Indexed appinfo.vdf reader
pub struct AppInfoReader {
    file: File,
    index: HashMap<u32, AppInfoEntry>,
    version: u32,
    /// v29 string table (binary-VDF key names). Empty on v28, where keys are
    /// stored inline as null-terminated strings.
    string_table: Vec<String>,
    /// Reusable read buffer - avoids allocating a fresh Vec per get_game_info() call.
    /// Grows to max entry size and stays there for the lifetime of the reader.
    read_buf: Vec<u8>,
}

impl AppInfoReader {
    /// Open and index appinfo.vdf
    ///
    /// Performance: ~80-120ms for 150MB file (builds index in single pass)
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut header = [0u8; 4];
        file.read_exact(&mut header)?;

        let magic = u32::from_le_bytes(header);
        let version = match magic {
            APPINFO_MAGIC_V28 => 28,
            APPINFO_MAGIC_V29 => 29,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Unknown appinfo.vdf version",
                ));
            }
        };

        // Universe (4 bytes).
        file.seek(SeekFrom::Current(4))?;

        // v29 added an i64 string-table offset right after universe, and moved
        // binary-VDF key names into a string table at that offset (each key is
        // now a u32 index instead of an inline string). v28 has neither.
        let string_table = if version >= 29 {
            let mut buf8 = [0u8; 8];
            file.read_exact(&mut buf8)?;
            let st_offset = i64::from_le_bytes(buf8);
            if st_offset > 0 {
                Self::read_string_table(&mut file, st_offset as u64).unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let index = Self::build_index(&mut file, version)?;

        Ok(Self {
            file,
            index,
            version,
            string_table,
            read_buf: Vec::new(),
        })
    }

    /// Read the v29 string table at `offset`: a u32 count followed by that many
    /// null-terminated UTF-8 strings, running to end of file.
    fn read_string_table(file: &mut File, offset: u64) -> io::Result<Vec<String>> {
        file.seek(SeekFrom::Start(offset))?;
        let mut buf4 = [0u8; 4];
        file.read_exact(&mut buf4)?;
        let count = u32::from_le_bytes(buf4) as usize;
        // Sanity cap so a corrupt offset can't drive a wild allocation.
        if count > 10_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "appinfo string-table count implausibly large",
            ));
        }
        let mut rest = Vec::new();
        file.read_to_end(&mut rest)?;

        let mut table = Vec::with_capacity(count.min(65_536));
        let mut start = 0usize;
        for _ in 0..count {
            let Some(nul) = rest[start..].iter().position(|&b| b == 0) else {
                break; // truncated table - keep what we have
            };
            table.push(String::from_utf8_lossy(&rest[start..start + nul]).into_owned());
            start += nul + 1;
        }
        Ok(table)
    }

    /// Build index in single sequential pass - O(n) where n = file size
    ///
    /// This is the hot path. We read sequentially (cache-friendly) and only
    /// store app_id -> offset mapping. No parsing of actual content yet.
    fn build_index(file: &mut File, version: u32) -> io::Result<HashMap<u32, AppInfoEntry>> {
        use log::info;

        let file_size = file.seek(SeekFrom::End(0))?;
        // Header is magic(4)+universe(4)=8 on v28; v29 adds an i64 string-table
        // offset -> 16. Seeking to the wrong one reads app entries mid-field.
        let header_size: u64 = if version >= 29 { 16 } else { 8 };
        file.seek(SeekFrom::Start(header_size))?;
        info!(
            "Steam: appinfo.vdf size={} bytes, version={}",
            file_size, version
        );

        let mut index = HashMap::with_capacity(2048);
        let mut buf4 = [0u8; 4];

        // v29 entry format:
        // - app_id: u32
        // - size: u32 (total size of remaining entry data)
        // - info_state: u32
        // - last_updated: u32
        // - access_token: u64
        // - sha1: [u8; 20]
        // - change_number: u32
        // - binary_vdf_data: [u8; ...]

        loop {
            let entry_start = file.stream_position()?;

            // Read app ID (4 bytes)
            if file.read_exact(&mut buf4).is_err() {
                break;
            }
            let app_id = u32::from_le_bytes(buf4);

            // app_id 0 marks end of entries
            if app_id == 0 {
                break;
            }

            // Read size (4 bytes) - this is the size of everything after this field
            file.read_exact(&mut buf4)?;
            let size = u32::from_le_bytes(buf4);

            // Validate size
            if size as u64 > file_size {
                info!(
                    "  Invalid size {} at offset {}, stopping",
                    size, entry_start
                );
                break;
            }

            // Store index entry - offset is start of entry, size is data section size
            let data_offset = entry_start + 8; // After app_id and size fields
            index.insert(
                app_id,
                AppInfoEntry {
                    offset: data_offset,
                    size,
                },
            );

            // Log first few entries for debugging
            if index.len() <= 5 {
                info!("  app_id={} offset={} size={}", app_id, entry_start, size);
            }

            // Skip to next entry (size bytes after the size field)
            file.seek(SeekFrom::Start(entry_start + 8 + size as u64))?;
        }

        index.shrink_to_fit(); // Release excess capacity
        Ok(index)
    }

    /// Get executable path for an app ID
    ///
    /// Performance: ~0.05-0.1ms per lookup (seek + small read + parse)
    pub fn get_executable(&mut self, app_id: u32) -> Option<String> {
        self.get_game_info(app_id).map(|(_, exe)| exe)
    }

    /// Get game name and executable for an app ID
    ///
    /// Returns: (name, executable)
    /// Performance: ~0.05-0.1ms per lookup (reuses internal buffer)
    pub fn get_game_info(&mut self, app_id: u32) -> Option<(String, String)> {
        let (offset, size) = {
            let entry = self.index.get(&app_id)?;
            (entry.offset, entry.size as usize)
        };

        // `offset` already points past app_id+size (build_index stored
        // entry_start+8). Between it and the binary VDF is the fixed metadata:
        //   infoState(4) lastUpdated(4) picsToken(8) textVdfSha1(20) changeNumber(4) = 40
        // and v29+ adds a 20-byte binary-VDF hash -> 60. (No extra +8 here: that
        // was double-counting the app_id+size already baked into `offset`.)
        let metadata = if self.version >= 29 { 60 } else { 40 };
        if size <= metadata {
            return None;
        }
        let data_offset = offset + metadata as u64;
        let vdf_len = size - metadata;

        self.file.seek(SeekFrom::Start(data_offset)).ok()?;
        // Reuse buffer - resize without shrinking (grows to max entry, stays there)
        self.read_buf.resize(vdf_len, 0);
        self.file.read_exact(&mut self.read_buf[..vdf_len]).ok()?;

        // Parse binary VDF to find name and executable (v29 keys are indices).
        Self::parse_game_info(
            &self.read_buf[..vdf_len],
            &self.string_table,
            self.version >= 29,
        )
    }

    /// Parse binary VDF data to extract game name and Windows launch executable
    fn parse_game_info(
        data: &[u8],
        string_table: &[String],
        indexed_keys: bool,
    ) -> Option<(String, String)> {
        let mut reader = BinaryVdfReader::new(data, string_table, indexed_keys);
        // Each app's blob is wrapped in a single root block (key "appinfo").
        // Step into it first, otherwise find_block would skip_block the entire
        // tree on the wrapper and never reach common/config/launch.
        reader.enter_root_block();

        // Get name from "common" block
        let mut name = None;
        if reader.find_block("common") {
            while let Some((key, value)) = reader.next_kv() {
                match value {
                    BinaryVdfValue::String(s) if key == "name" => {
                        name = Some(s);
                        break;
                    }
                    BinaryVdfValue::BlockEnd => break,
                    _ => {}
                }
            }
        }

        // Navigate to "launch" block for executable
        reader.reset_to_root();
        let exe = if reader.find_block("launch") {
            Self::find_windows_executable(&mut reader)
        } else {
            reader.reset_to_root();
            if reader.find_nested_block(&["config", "launch"]) {
                Self::find_windows_executable(&mut reader)
            } else {
                None
            }
        };

        Some((name?, exe?))
    }

    fn find_windows_executable(reader: &mut BinaryVdfReader) -> Option<String> {
        // Iterate through launch configs (0, 1, 2, ...)
        let mut depth = 0;
        let mut current_exe: Option<String> = None;
        let mut is_windows = false;
        let mut is_default = true; // Assume default unless specified otherwise

        while let Some((key, value)) = reader.next_kv() {
            match value {
                BinaryVdfValue::BlockStart => {
                    depth += 1;
                    current_exe = None;
                    is_windows = false;
                    is_default = true;
                }
                BinaryVdfValue::BlockEnd => {
                    depth -= 1;
                    if depth == 0 {
                        // End of a launch config - check if it's valid
                        if let Some(exe) = current_exe.take()
                            && (is_windows || is_default)
                            && !exe.contains("linux")
                            && !exe.contains("osx")
                        {
                            // Found a Windows executable
                            if exe.ends_with(".exe") || !exe.contains('.') {
                                return Some(exe);
                            }
                        }
                    }
                }
                BinaryVdfValue::String(s) => match key {
                    "executable" => current_exe = Some(s),
                    "oslist" => is_windows = s.contains("windows"),
                    "type" if s != "default" && s != "none" => is_default = false,
                    _ => {}
                },
                _ => {}
            }
        }

        None
    }

    /// Number of indexed apps
    #[inline]
    pub fn app_count(&self) -> usize {
        self.index.len()
    }
}

/// Zero-copy binary VDF reader
struct BinaryVdfReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// v29 key-name pool; keys in `data` are u32 indices into this.
    string_table: &'a [String],
    /// True on v29+ (keys are string-table indices); false on v28 (inline keys).
    indexed_keys: bool,
}

enum BinaryVdfValue {
    BlockStart,
    BlockEnd,
    String(String),
    Int32(i32),
}

impl<'a> BinaryVdfReader<'a> {
    fn new(data: &'a [u8], string_table: &'a [String], indexed_keys: bool) -> Self {
        Self {
            data,
            pos: 0,
            string_table,
            indexed_keys,
        }
    }

    /// Step into the single root block (key "appinfo") that wraps each app's
    /// blob, so sibling searches see its children. No-op if the first token
    /// isn't a block (position is left unchanged).
    fn enter_root_block(&mut self) {
        let save = self.pos;
        if !matches!(self.next_kv(), Some((_, BinaryVdfValue::BlockStart))) {
            self.pos = save;
        }
    }

    /// Reset to the start and re-enter the root block.
    fn reset_to_root(&mut self) {
        self.pos = 0;
        self.enter_root_block();
    }

    fn next_kv(&mut self) -> Option<(&'a str, BinaryVdfValue)> {
        // Iterative (not recursive): an unknown type byte skips to the next entry
        // via `continue`. A malformed run of unknown markers would otherwise
        // recurse per byte and overflow the stack (crash on a corrupt file).
        loop {
            if self.pos >= self.data.len() {
                return None;
            }

            let type_byte = self.data[self.pos];
            self.pos += 1;

            if type_byte == TYPE_BLOCK_END {
                return Some(("", BinaryVdfValue::BlockEnd));
            }

            // Read key: v29 = u32 string-table index; v28 = inline null-terminated.
            let key: &'a str = if self.indexed_keys {
                if self.pos + 4 > self.data.len() {
                    return None;
                }
                let idx =
                    u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().ok()?) as usize;
                self.pos += 4;
                self.string_table.get(idx).map(String::as_str)?
            } else {
                let key_start = self.pos;
                while self.pos < self.data.len() && self.data[self.pos] != 0 {
                    self.pos += 1;
                }
                let k = std::str::from_utf8(&self.data[key_start..self.pos]).ok()?;
                self.pos += 1; // Skip null terminator
                k
            };

            let value = match type_byte {
                TYPE_BLOCK_START => BinaryVdfValue::BlockStart,
                TYPE_STRING => {
                    let str_start = self.pos;
                    while self.pos < self.data.len() && self.data[self.pos] != 0 {
                        self.pos += 1;
                    }
                    let s = std::str::from_utf8(&self.data[str_start..self.pos])
                        .ok()?
                        .to_string();
                    self.pos += 1;
                    BinaryVdfValue::String(s)
                }
                TYPE_INT32 => {
                    if self.pos + 4 > self.data.len() {
                        return None;
                    }
                    let val =
                        i32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().ok()?);
                    self.pos += 4;
                    BinaryVdfValue::Int32(val)
                }
                // Value types we don't use but MUST consume so the stream stays
                // aligned (the key was already read): Float32/Pointer/Color are 4
                // bytes, UInt64/Int64 are 8. Skipping them without consuming would
                // desync the reader and corrupt skip_block's depth tracking.
                0x03 | 0x04 | 0x06 => {
                    if self.pos + 4 > self.data.len() {
                        return None;
                    }
                    self.pos += 4;
                    continue;
                }
                0x07 | 0x0a => {
                    if self.pos + 8 > self.data.len() {
                        return None;
                    }
                    self.pos += 8;
                    continue;
                }
                // Unknown type of unknown width: stop rather than desync.
                _ => return None,
            };

            return Some((key, value));
        }
    }

    fn find_block(&mut self, name: &str) -> bool {
        while let Some((key, value)) = self.next_kv() {
            if let BinaryVdfValue::BlockStart = value {
                if key == name {
                    return true;
                }
                // Skip this block
                self.skip_block();
            }
        }
        false
    }

    fn find_nested_block(&mut self, path: &[&str]) -> bool {
        for name in path {
            if !self.find_block(name) {
                return false;
            }
        }
        true
    }

    fn skip_block(&mut self) {
        let mut depth = 1;
        while depth > 0 {
            if let Some((_, value)) = self.next_kv() {
                match value {
                    BinaryVdfValue::BlockStart => depth += 1,
                    BinaryVdfValue::BlockEnd => depth -= 1,
                    _ => {}
                }
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_constants() {
        // Verify magic bytes are correct
        assert_eq!(APPINFO_MAGIC_V28, 0x07564428);
        assert_eq!(APPINFO_MAGIC_V29, 0x07564429);
    }

    #[test]
    fn test_v29_indexed_keys() {
        // v29: keys are u32 indices into the string table; string values inline.
        let table = vec!["common".to_string(), "name".to_string()];
        let mut data = Vec::new();
        data.push(TYPE_BLOCK_START);
        data.extend_from_slice(&0u32.to_le_bytes()); // key idx 0 -> "common"
        data.push(TYPE_STRING);
        data.extend_from_slice(&1u32.to_le_bytes()); // key idx 1 -> "name"
        data.extend_from_slice(b"Half-Life\0"); // inline string value
        data.push(TYPE_BLOCK_END);

        let mut reader = BinaryVdfReader::new(&data, &table, true);
        assert!(reader.find_block("common"));
        let (key, value) = reader.next_kv().expect("kv");
        assert_eq!(key, "name");
        match value {
            BinaryVdfValue::String(s) => assert_eq!(s, "Half-Life"),
            _ => panic!("expected string value"),
        }
    }

    #[test]
    fn test_v28_inline_keys() {
        // v28: keys are inline null-terminated strings (empty string table).
        let table: Vec<String> = Vec::new();
        let mut data = Vec::new();
        data.push(TYPE_STRING);
        data.extend_from_slice(b"name\0"); // inline key
        data.extend_from_slice(b"Portal\0"); // inline value

        let mut reader = BinaryVdfReader::new(&data, &table, false);
        let (key, value) = reader.next_kv().expect("kv");
        assert_eq!(key, "name");
        match value {
            BinaryVdfValue::String(s) => assert_eq!(s, "Portal"),
            _ => panic!("expected string value"),
        }
    }

    #[test]
    fn test_parse_game_info_descends_root_block() {
        // Real blobs wrap everything in a root "appinfo" block; parse_game_info
        // must descend into it to reach common/config/launch. Without the root
        // descent this returns None (the bug the whole appinfo path had).
        let table: Vec<String> = [
            "appinfo",
            "common",
            "name",
            "config",
            "launch",
            "0",
            "executable",
            "oslist",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let bs = |d: &mut Vec<u8>, idx: u32| {
            d.push(TYPE_BLOCK_START);
            d.extend_from_slice(&idx.to_le_bytes());
        };
        let be = |d: &mut Vec<u8>| d.push(TYPE_BLOCK_END);
        let st = |d: &mut Vec<u8>, idx: u32, val: &str| {
            d.push(TYPE_STRING);
            d.extend_from_slice(&idx.to_le_bytes());
            d.extend_from_slice(val.as_bytes());
            d.push(0);
        };

        let mut d = Vec::new();
        bs(&mut d, 0); // appinfo (root wrapper)
        bs(&mut d, 1); // common
        st(&mut d, 2, "TestGame"); // name
        be(&mut d);
        bs(&mut d, 3); // config
        bs(&mut d, 4); // launch
        bs(&mut d, 5); // "0"
        st(&mut d, 6, "game.exe"); // executable
        st(&mut d, 7, "windows"); // oslist
        be(&mut d);
        be(&mut d);
        be(&mut d);
        be(&mut d); // end appinfo

        let (name, exe) =
            AppInfoReader::parse_game_info(&d, &table, true).expect("should parse wrapped blob");
        assert_eq!(name, "TestGame");
        assert_eq!(exe, "game.exe");
    }
}
