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

/// Mirrors Steam's `AppUpdateInfo_s`. `#[repr(C)]` reproduces the C padding (4 bytes
/// after the leading u32 so the u64s are 8-aligned).
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
    // SAFETY: loading the platform's own steamclient; dependent libs resolve via the
    // search path the parent set when spawning this probe.
    let lib = unsafe { libloading::Library::new(&lib_path) }.map_err(|_| "load failed")?;
    let create_interface: libloading::Symbol<CreateInterfaceFn> =
        unsafe { lib.get(b"CreateInterface\0") }.map_err(|_| "no CreateInterface")?;

    unsafe {
        // 1) IClientEngine (try the known version, else scan the DLL for the current).
        let default_ver = CString::new(DEFAULT_ENGINE_VERSION).unwrap();
        let mut engine = create_interface(default_ver.as_ptr(), std::ptr::null_mut());
        if engine.is_null()
            && let Some(scanned) = scan_engine_version(&lib_path)
        {
            engine = create_interface(scanned.as_ptr(), std::ptr::null_mut());
        }
        if engine.is_null() {
            return Err("no client engine");
        }

        // 2) Steam pipe + global user (early, stable slots).
        let create_pipe: FnPipe = std::mem::transmute(vfn(engine, SLOT_CREATE_STEAM_PIPE));
        let pipe = create_pipe(engine);
        if pipe == 0 {
            return Err("no steam pipe");
        }
        let connect_user: FnConnect = std::mem::transmute(vfn(engine, SLOT_CONNECT_GLOBAL_USER));
        let user = connect_user(engine, pipe);
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
        if !looks_like_interface(appmgr) {
            return Err("app manager slot invalid");
        }

        // 4) Which app (if any) is downloading, then its byte counts.
        let get_downloading: FnDownloadingId =
            std::mem::transmute(vfn(appmgr, SLOT_GET_DOWNLOADING_APP_ID));
        let appid = get_downloading(appmgr);
        if appid == 0 {
            return Ok(None); // connected, nothing downloading
        }

        let get_update_info: FnUpdateInfo = std::mem::transmute(vfn(appmgr, SLOT_GET_UPDATE_INFO));
        let mut info = AppUpdateInfo::default();
        get_update_info(appmgr, appid, &raw mut info);

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
        Err(_) => {} // no output -> parent treats as unavailable
    }
}
