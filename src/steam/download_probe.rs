//! Live Steam download progress via steamclient's private `IClientAppManager`.
//!
//! This is READ-ONLY (`GetUpdateInfo` / `GetDownloadingAppID`) and rides the Steam
//! pipe that every Steamworks app already uses, so it opens no new attack surface
//! (unlike the CEF debug port) and is not an anti-cheat concern (it never touches a
//! game process).
//!
//! It runs ONLY in an isolated subprocess (`pc-bridge --steam-download-probe`):
//! calling a private C++ vtable by slot index is undefined behavior if a Steam
//! update reorders methods, so a wrong slot must be able to crash a THROWAWAY
//! process, never the agent. The parent treats no/garbage output as "unavailable".
//!
//! Vtable slots (from SteamRE reverse engineering):
//! - IClientEngine: `CreateSteamPipe`=0, `ConnectToGlobalUser`=3 (both stable),
//!   `GetIClientAppManager`=36 (this one DRIFTS across engine versions - validated
//!   at runtime, never blindly trusted).
//! - IClientAppManager: `GetDownloadingAppID`=30, `GetUpdateInfo`=13 (both stable:
//!   early slots in a focused interface where Valve only appends).

use std::ffi::{CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};

const DEFAULT_ENGINE_VERSION: &str = "CLIENTENGINE_INTERFACE_VERSION005";
const ENGINE_VERSION_PREFIX: &[u8] = b"CLIENTENGINE_INTERFACE_VERSION";
const APPMANAGER_VERSION: &str = "IClientAppManager";

const SLOT_CREATE_STEAM_PIPE: usize = 0;
const SLOT_CONNECT_GLOBAL_USER: usize = 3;
/// Candidate slot for `GetIClientAppManager` - drifts across engine versions, so the
/// result is pointer-validated before use (swap in dynamic resolution here later
/// without touching the stable slots).
const SLOT_GET_ICLIENT_APP_MANAGER: usize = 36;
const SLOT_GET_DOWNLOADING_APP_ID: usize = 30;
const SLOT_GET_UPDATE_INFO: usize = 13;

/// Mirrors the LEADING fields of Steam's `AppUpdateInfo_s`. `#[repr(C)]` reproduces
/// the C padding (4 bytes after the leading u32 so the u64s are 8-aligned).
#[repr(C)]
#[derive(Default)]
struct AppUpdateInfo {
    time_update_start: u32,
    bytes_to_download: u64,
    bytes_downloaded: u64,
    bytes_to_process: u64,
    bytes_processed: u64,
    unk: u32,
}

/// Receive buffer for `GetUpdateInfo`. The callee writes however large the CURRENT
/// (private, undocumented) `AppUpdateInfo_s` is; if Steam appended fields, a 48-byte
/// receiver would be overflowed (stack corruption -> wrong data or crash). Over-
/// allocate generous 8-aligned headroom so any growth lands in `headroom`, and read
/// only the leading fields we understand.
#[repr(C)]
struct AppUpdateInfoBuf {
    info: AppUpdateInfo,
    #[allow(dead_code)]
    headroom: [u64; 24], // 192 spare bytes
}

type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
type FnPipe = unsafe extern "C" fn(*mut c_void) -> i32;
type FnConnect = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type FnGetAppMgr = unsafe extern "C" fn(*mut c_void, i32, i32, *const c_char) -> *mut c_void;
type FnDownloadingId = unsafe extern "C" fn(*mut c_void) -> u32;
type FnUpdateInfo = unsafe extern "C" fn(*mut c_void, u32, *mut AppUpdateInfo) -> u32;

/// Read the slot-th function pointer out of a C++ object's vtable.
///
/// # Safety
/// `obj` must point to an object whose first machine word is a vtable pointer with
/// at least `slot + 1` entries.
unsafe fn vfn(obj: *mut c_void, slot: usize) -> *const c_void {
    unsafe {
        let vtable: *const *const c_void = *(obj as *const *const *const c_void);
        *vtable.add(slot)
    }
}

/// True if `obj` looks like a live interface pointer (non-null with a non-null
/// vtable). Cheap guard against an obviously-wrong `GetIClientAppManager` slot.
///
/// # Safety
/// `obj` is treated as a possible interface pointer; only its first word is read.
unsafe fn looks_like_interface(obj: *mut c_void) -> bool {
    if obj.is_null() {
        return false;
    }
    unsafe {
        let vtable = *(obj as *const *const c_void);
        !vtable.is_null()
    }
}

/// Path to the platform's steamclient library.
pub fn steamclient_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let candidates: Vec<PathBuf> = {
        let home = std::env::var("HOME").ok()?;
        vec![PathBuf::from(home).join(
            "Library/Application Support/Steam/Steam.AppBundle/Steam/Contents/MacOS/steamclient.dylib",
        )]
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: Vec<PathBuf> = {
        let steam = crate::steam::find_steam_path()?;
        vec![
            steam.join("linux64/steamclient.so"),
            steam.join("ubuntu12_64/steamclient.so"),
        ]
    };
    #[cfg(windows)]
    let candidates: Vec<PathBuf> = {
        let steam = crate::steam::find_steam_path()?;
        vec![steam.join("steamclient64.dll")]
    };

    candidates.into_iter().find(|p| p.exists())
}

/// Directory holding steamclient's dependent libraries, so the parent can add it to
/// the probe's library search path when spawning.
pub fn steamclient_dir() -> Option<PathBuf> {
    steamclient_path().and_then(|p| p.parent().map(PathBuf::from))
}

/// Add the steamclient directory to this process's DLL search path so its sibling
/// dependencies load regardless of cwd (also makes manual probe runs work anywhere).
#[cfg(windows)]
fn add_dll_dir(lib_path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let Some(dir) = lib_path.parent() else { return };
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: valid null-terminated wide path; SetDllDirectoryW just sets a search dir.
    unsafe {
        let _ = windows::Win32::System::LibraryLoader::SetDllDirectoryW(windows::core::PCWSTR(
            wide.as_ptr(),
        ));
    }
}

/// Scan the library on disk for its current `CLIENTENGINE_INTERFACE_VERSION###`.
/// Only used as a fallback when the default version doesn't resolve (Steam bumped
/// it), so the common path never reads the whole (large) library.
fn scan_engine_version(lib_path: &Path) -> Option<CString> {
    let bytes = std::fs::read(lib_path).ok()?;
    let pos = bytes
        .windows(ENGINE_VERSION_PREFIX.len())
        .position(|w| w == ENGINE_VERSION_PREFIX)?;
    let mut end = pos + ENGINE_VERSION_PREFIX.len();
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    CString::new(&bytes[pos..end]).ok()
}

struct Progress {
    appid: u32,
    downloaded: u64,
    total: u64,
}

/// Bootstrap steamclient and read the currently-downloading app's progress.
/// `Ok(Some)` while downloading, `Ok(None)` when connected but idle, `Err` when the
/// client couldn't be reached at all.
fn probe() -> Result<Option<Progress>, &'static str> {
    let lib_path = steamclient_path().ok_or("steamclient not found")?;
    eprintln!("[probe] steamclient: {}", lib_path.display());
    // On Windows, add the steamclient dir to the DLL search path so its sibling
    // dependencies resolve no matter the cwd (helps manual runs too).
    #[cfg(windows)]
    add_dll_dir(&lib_path);
    // SAFETY: loading the platform's own steamclient; dependent libs resolve via the
    // search path above / the env the parent set when spawning this probe.
    let lib = unsafe { libloading::Library::new(&lib_path) }.map_err(|_| "load failed")?;
    eprintln!("[probe] loaded steamclient");
    let create_interface: libloading::Symbol<CreateInterfaceFn> =
        unsafe { lib.get(b"CreateInterface\0") }.map_err(|_| "no CreateInterface")?;
    eprintln!("[probe] CreateInterface resolved");

    unsafe {
        // 1) IClientEngine (try the known version, else scan the DLL for the current).
        let default_ver = CString::new(DEFAULT_ENGINE_VERSION).unwrap();
        let mut engine = create_interface(default_ver.as_ptr(), std::ptr::null_mut());
        let mut ver_used = DEFAULT_ENGINE_VERSION.to_string();
        if engine.is_null()
            && let Some(scanned) = scan_engine_version(&lib_path)
        {
            ver_used = scanned.to_string_lossy().into_owned();
            engine = create_interface(scanned.as_ptr(), std::ptr::null_mut());
        }
        eprintln!(
            "[probe] engine ({ver_used}): {}",
            if engine.is_null() { "NULL" } else { "ok" }
        );
        if engine.is_null() {
            return Err("no client engine");
        }

        // 2) Steam pipe + global user (early, stable slots).
        let create_pipe: FnPipe = std::mem::transmute(vfn(engine, SLOT_CREATE_STEAM_PIPE));
        let pipe = create_pipe(engine);
        eprintln!("[probe] steam pipe: {pipe}");
        if pipe == 0 {
            return Err("no steam pipe");
        }
        let connect_user: FnConnect = std::mem::transmute(vfn(engine, SLOT_CONNECT_GLOBAL_USER));
        let user = connect_user(engine, pipe);
        eprintln!("[probe] global user: {user}");
        if user == 0 {
            return Err("no global user (steam not running?)");
        }

        // 3) IClientAppManager (drifting slot - pass the version string, harmless for
        //    both the 2- and 3-arg engine variants on x64, then validate the pointer
        //    before ever calling through it).
        let appmgr_ver = CString::new(APPMANAGER_VERSION).unwrap();
        let get_app_manager: FnGetAppMgr =
            std::mem::transmute(vfn(engine, SLOT_GET_ICLIENT_APP_MANAGER));
        let appmgr = get_app_manager(engine, user, pipe, appmgr_ver.as_ptr());
        eprintln!(
            "[probe] app manager (slot {SLOT_GET_ICLIENT_APP_MANAGER}) valid: {}",
            looks_like_interface(appmgr)
        );
        if !looks_like_interface(appmgr) {
            return Err("app manager slot invalid");
        }

        // 4) Which app (if any) is downloading, then its byte counts.
        let get_downloading: FnDownloadingId =
            std::mem::transmute(vfn(appmgr, SLOT_GET_DOWNLOADING_APP_ID));
        let appid = get_downloading(appmgr);
        eprintln!("[probe] downloading appid: {appid}");
        if appid == 0 {
            return Ok(None); // connected, nothing downloading
        }

        let get_update_info: FnUpdateInfo = std::mem::transmute(vfn(appmgr, SLOT_GET_UPDATE_INFO));
        // Over-allocated receiver (see AppUpdateInfoBuf): the callee may write more
        // than our 48-byte view if the private struct grew; the extra lands in slack.
        let mut buf: AppUpdateInfoBuf = std::mem::zeroed();
        get_update_info(appmgr, appid, (&raw mut buf).cast::<AppUpdateInfo>());
        let info = &buf.info;
        eprintln!(
            "[probe] appid {appid}: downloaded={} to_download={} to_process={}",
            info.bytes_downloaded, info.bytes_to_download, info.bytes_to_process
        );

        // Sanity-check before trusting a struct read through a private ABI.
        let sane_max = 10_u64 << 40; // 10 TB
        if info.bytes_to_download == 0
            || info.bytes_to_download > sane_max
            || info.bytes_downloaded > info.bytes_to_download
        {
            return Ok(None);
        }
        Ok(Some(Progress {
            appid,
            downloaded: info.bytes_downloaded,
            total: info.bytes_to_download,
        }))
    }
}

/// Subcommand entry: run the probe and print one JSON line to stdout.
/// - downloading: `{"appid":N,"downloaded":D,"total":T}`
/// - connected, idle: `{"idle":true}`
/// - couldn't reach the client: prints nothing (parent -> "unavailable").
pub fn run_and_print() {
    match probe() {
        Ok(Some(p)) => println!(
            "{{\"appid\":{},\"downloaded\":{},\"total\":{}}}",
            p.appid, p.downloaded, p.total
        ),
        Ok(None) => println!("{{\"idle\":true}}"),
        Err(e) => eprintln!("[probe] failed: {e}"), // stderr only; stdout stays empty
    }
}
