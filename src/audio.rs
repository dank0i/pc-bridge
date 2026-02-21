//! Audio control - Volume, mute, media keys
//!
//! Uses Windows Core Audio API (IAudioEndpointVolume) for volume control.
//! Registers an IMMNotificationClient to detect default audio device changes
//! and automatically invalidate the cached endpoint.
//!
//! No PowerShell, no external processes.
#![allow(dead_code)] // Used on Windows only

#[cfg(windows)]
use std::cell::Cell;
#[cfg(windows)]
use std::cell::RefCell;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use windows::{
    Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    Win32::Media::Audio::{
        EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
        MMDeviceEnumerator, eConsole, eRender,
    },
    Win32::System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx},
};

/// Signalled by the IMMNotificationClient callback when the default audio
/// device changes.  Checked (and cleared) before using the cached endpoint.
#[cfg(windows)]
static DEVICE_CHANGED: AtomicBool = AtomicBool::new(false);

/// Ensures the notification listener is registered exactly once (process-wide).
#[cfg(windows)]
static LISTENER_REGISTERED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
thread_local! {
    /// COM initialization state — must be per-thread (STA).
    static COM_INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(windows)]
thread_local! {
    /// Cached audio endpoint volume interface — avoids recreating 3 COM objects per call.
    /// Invalidated on any COM error or when the default audio device changes.
    static CACHED_ENDPOINT: RefCell<Option<IAudioEndpointVolume>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// IMMNotificationClient implementation — receives device-change callbacks
// from the Windows audio subsystem on a COM background thread.
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[windows::core::implement(IMMNotificationClient)]
struct DeviceChangeListener;

#[cfg(windows)]
impl IMMNotificationClient_Impl for DeviceChangeListener_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _pwstrdefaultdeviceid: &windows::core::PCWSTR,
    ) -> windows::core::Result<()> {
        log::debug!("Default audio device changed — will rebuild endpoint cache");
        DEVICE_CHANGED.store(true, Ordering::Release);
        Ok(())
    }

    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &windows::core::PCWSTR,
        _dwnewstate: windows::Win32::Media::Audio::DEVICE_STATE,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &windows::core::PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &windows::core::PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &windows::core::PCWSTR,
        _key: &windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Register the device-change notification listener (once per process).
/// Must be called from a COM-initialized thread.
#[cfg(windows)]
fn register_device_listener() {
    if LISTENER_REGISTERED.swap(true, Ordering::AcqRel) {
        return; // Already registered
    }

    unsafe {
        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("Failed to create device enumerator for listener: {e}");
                    LISTENER_REGISTERED.store(false, Ordering::Release);
                    return;
                }
            };

        let listener: IMMNotificationClient = DeviceChangeListener.into();
        if let Err(e) = enumerator.RegisterEndpointNotificationCallback(&listener) {
            log::warn!("Failed to register audio device listener: {e}");
            LISTENER_REGISTERED.store(false, Ordering::Release);
        } else {
            // Intentionally leak the listener and enumerator — they live for the
            // process lifetime.  COM prevent prevent ref-count drop to zero.
            std::mem::forget(listener);
            std::mem::forget(enumerator);
            log::info!("Audio device change listener registered");
        }
    }
}

/// Initialize COM once for the current thread (STA)
#[cfg(windows)]
fn ensure_com_init() {
    COM_INITIALIZED.with(|init| {
        if !init.get() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            init.set(true);
        }
    });
    // Register device-change listener (idempotent, process-wide)
    register_device_listener();
}

/// Get (or create and cache) the default audio endpoint volume interface.
/// Clone just increments the COM reference count — effectively free.
///
/// Automatically rebuilds the cache if the default audio device changed
/// since the last call (signalled by the [`DeviceChangeListener`]).
#[cfg(windows)]
fn get_endpoint_volume() -> Option<IAudioEndpointVolume> {
    ensure_com_init();

    // If the default device changed, drop the stale cache
    if DEVICE_CHANGED.swap(false, Ordering::Acquire) {
        log::info!("Default audio device changed — rebuilding endpoint cache");
        invalidate_endpoint_cache();
    }

    CACHED_ENDPOINT.with(|cell| {
        let cached = cell.borrow();
        if let Some(ref vol) = *cached {
            return Some(vol.clone());
        }
        drop(cached);

        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
            *cell.borrow_mut() = Some(volume.clone());
            Some(volume)
        }
    })
}

/// Invalidate the cached endpoint (called on COM errors so next call creates fresh).
#[cfg(windows)]
fn invalidate_endpoint_cache() {
    CACHED_ENDPOINT.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Get current system volume (0-100)
#[cfg(windows)]
pub fn get_volume() -> Option<f32> {
    let volume = get_endpoint_volume()?;
    match unsafe { volume.GetMasterVolumeLevelScalar() } {
        Ok(level) => Some(level * 100.0),
        Err(_) => {
            invalidate_endpoint_cache();
            None
        }
    }
}

/// Set system volume (0-100)
#[cfg(windows)]
pub fn set_volume(level: f32) -> bool {
    let Some(volume) = get_endpoint_volume() else {
        return false;
    };
    let scalar = (level / 100.0).clamp(0.0, 1.0);
    match unsafe { volume.SetMasterVolumeLevelScalar(scalar, std::ptr::null()) } {
        Ok(()) => true,
        Err(_) => {
            invalidate_endpoint_cache();
            false
        }
    }
}

/// Get mute status
#[cfg(windows)]
pub fn get_mute() -> Option<bool> {
    let volume = get_endpoint_volume()?;
    match unsafe { volume.GetMute() } {
        Ok(muted) => Some(muted.as_bool()),
        Err(_) => {
            invalidate_endpoint_cache();
            None
        }
    }
}

/// Set mute status
#[cfg(windows)]
pub fn set_mute(mute: bool) -> bool {
    let Some(volume) = get_endpoint_volume() else {
        return false;
    };
    match unsafe { volume.SetMute(mute, std::ptr::null()) } {
        Ok(()) => true,
        Err(_) => {
            invalidate_endpoint_cache();
            false
        }
    }
}

/// Toggle mute
#[cfg(windows)]
pub fn toggle_mute() -> bool {
    if let Some(muted) = get_mute() {
        set_mute(!muted)
    } else {
        false
    }
}

/// Send media key press (play/pause, next, previous, stop)
#[cfg(windows)]
pub fn send_media_key(key: MediaKey) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
        VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP,
    };

    let vk = match key {
        MediaKey::PlayPause => VK_MEDIA_PLAY_PAUSE,
        MediaKey::Next => VK_MEDIA_NEXT_TRACK,
        MediaKey::Previous => VK_MEDIA_PREV_TRACK,
        MediaKey::Stop => VK_MEDIA_STOP,
        MediaKey::VolumeUp => VK_VOLUME_UP,
        MediaKey::VolumeDown => VK_VOLUME_DOWN,
        MediaKey::VolumeMute => VK_VOLUME_MUTE,
    };

    unsafe {
        let mut input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        // Key down
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);

        // Key up
        input.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum MediaKey {
    PlayPause,
    Next,
    Previous,
    Stop,
    VolumeUp,
    VolumeDown,
    VolumeMute,
}

// ============================================================================
// Linux implementations
// ============================================================================

#[cfg(unix)]
pub fn get_volume() -> Option<f32> {
    // Use pactl or amixer
    if let Ok(output) = std::process::Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse "Volume: front-left: 65536 / 100%"
        if let Some(pos) = stdout.find('%') {
            let start = stdout[..pos].rfind(' ').map(|i| i + 1).unwrap_or(0);
            if let Ok(vol) = stdout[start..pos].trim().parse::<f32>() {
                return Some(vol);
            }
        }
    }
    None
}

#[cfg(unix)]
pub fn set_volume(level: f32) -> bool {
    std::process::Command::new("pactl")
        .args([
            "set-sink-volume",
            "@DEFAULT_SINK@",
            &format!("{}%", level as u32),
        ])
        .spawn()
        .is_ok()
}

#[cfg(unix)]
pub fn get_mute() -> Option<bool> {
    if let Ok(output) = std::process::Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Some(stdout.contains("yes"));
    }
    None
}

#[cfg(unix)]
pub fn set_mute(mute: bool) -> bool {
    std::process::Command::new("pactl")
        .args([
            "set-sink-mute",
            "@DEFAULT_SINK@",
            if mute { "1" } else { "0" },
        ])
        .spawn()
        .is_ok()
}

#[cfg(unix)]
pub fn toggle_mute() -> bool {
    std::process::Command::new("pactl")
        .args(["set-sink-mute", "@DEFAULT_SINK@", "toggle"])
        .spawn()
        .is_ok()
}

#[cfg(unix)]
pub fn send_media_key(key: MediaKey) {
    let key_name = match key {
        MediaKey::PlayPause => "XF86AudioPlay",
        MediaKey::Next => "XF86AudioNext",
        MediaKey::Previous => "XF86AudioPrev",
        MediaKey::Stop => "XF86AudioStop",
        MediaKey::VolumeUp => "XF86AudioRaiseVolume",
        MediaKey::VolumeDown => "XF86AudioLowerVolume",
        MediaKey::VolumeMute => "XF86AudioMute",
    };

    let _ = std::process::Command::new("xdotool")
        .args(["key", key_name])
        .spawn();
}
