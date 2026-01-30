//! Audio control - Volume, mute, media keys
//!
//! Uses Windows Core Audio API (IAudioEndpointVolume) for volume control.
//! No PowerShell, no external processes.

#[cfg(windows)]
use windows::{
    core::*,
    Win32::Media::Audio::*,
    Win32::Media::Audio::Endpoints::*,
    Win32::System::Com::*,
};
use tracing::{debug, error};
use std::sync::OnceLock;

#[cfg(windows)]
static COM_INITIALIZED: OnceLock<()> = OnceLock::new();

/// Initialize COM once for the thread
#[cfg(windows)]
fn ensure_com_init() {
    COM_INITIALIZED.get_or_init(|| {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
    });
}

/// Get current system volume (0-100)
#[cfg(windows)]
pub fn get_volume() -> Option<f32> {
    ensure_com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator = 
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        
        let level = volume.GetMasterVolumeLevelScalar().ok()?;
        Some(level * 100.0)
        // COM objects dropped here via windows crate's Drop impl
    }
}

/// Set system volume (0-100)
#[cfg(windows)]
pub fn set_volume(level: f32) -> bool {
    ensure_com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(_) => return false,
        };
        
        let device = match enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
            Ok(d) => d,
            Err(_) => return false,
        };
        
        let volume: IAudioEndpointVolume = match device.Activate(CLSCTX_ALL, None) {
            Ok(v) => v,
            Err(_) => return false,
        };
        
        let scalar = (level / 100.0).clamp(0.0, 1.0);
        volume.SetMasterVolumeLevelScalar(scalar, std::ptr::null()).is_ok()
    }
}

/// Get mute status
#[cfg(windows)]
pub fn get_mute() -> Option<bool> {
    ensure_com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator = 
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        
        let muted = volume.GetMute().ok()?;
        Some(muted.as_bool())
    }
}

/// Set mute status
#[cfg(windows)]
pub fn set_mute(mute: bool) -> bool {
    ensure_com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(_) => return false,
        };
        
        let device = match enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
            Ok(d) => d,
            Err(_) => return false,
        };
        
        let volume: IAudioEndpointVolume = match device.Activate(CLSCTX_ALL, None) {
            Ok(v) => v,
            Err(_) => return false,
        };
        
        volume.SetMute(mute, std::ptr::null()).is_ok()
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
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

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
        .args(["set-sink-volume", "@DEFAULT_SINK@", &format!("{}%", level as u32)])
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
        .args(["set-sink-mute", "@DEFAULT_SINK@", if mute { "1" } else { "0" }])
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
