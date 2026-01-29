//! System tray icon with menu

#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use tokio::sync::broadcast;
#[cfg(windows)]
use tracing::{info, error, debug};

#[cfg(windows)]
use tray_icon::{TrayIconBuilder, Icon};
#[cfg(windows)]
use muda::{Menu, MenuItem, MenuEvent, PredefinedMenuItem};

#[cfg(windows)]
const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.ico");

/// Run the tray icon on a dedicated thread (blocking, uses Windows message loop)
#[cfg(windows)]
pub fn run_tray(shutdown_tx: broadcast::Sender<()>, config_path: std::path::PathBuf) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, TranslateMessage, DispatchMessageW, MSG,
    };

    info!("Starting system tray");

    // Build menu
    let menu = Menu::new();
    let open_config = MenuItem::new("Open Config", true, None);
    let separator = PredefinedMenuItem::separator();
    let exit = MenuItem::new("Exit", true, None);

    let open_config_id = open_config.id().clone();
    let exit_id = exit.id().clone();

    menu.append(&open_config).unwrap();
    menu.append(&separator).unwrap();
    menu.append(&exit).unwrap();

    // Load icon
    let icon = match load_icon() {
        Ok(i) => i,
        Err(e) => {
            error!("Failed to load tray icon: {}", e);
            return;
        }
    };

    // Create tray icon
    let _tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("PC Bridge")
        .with_icon(icon)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to create tray icon: {}", e);
            return;
        }
    };

    info!("Tray icon created");

    // Handle menu events in a separate thread
    let shutdown_tx_clone = shutdown_tx.clone();
    std::thread::spawn(move || {
        loop {
            if let Ok(event) = MenuEvent::receiver().recv() {
                if event.id == open_config_id {
                    debug!("Tray: Open Config clicked");
                    open_config_file(&config_path);
                } else if event.id == exit_id {
                    info!("Tray: Exit clicked");
                    let _ = shutdown_tx_clone.send(());
                    break;
                }
            }
        }
    });

    // Windows message loop (keeps tray alive)
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == 0 || ret.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Check if we should exit
            if shutdown_tx.receiver_count() == 0 {
                break;
            }
        }
    }

    debug!("Tray message loop ended");
}

#[cfg(windows)]
fn load_icon() -> anyhow::Result<Icon> {
    let (icon_rgba, icon_width, icon_height) = decode_ico(ICON_BYTES)?;
    Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .map_err(|e| anyhow::anyhow!("Failed to create icon: {}", e))
}

#[cfg(windows)]
fn decode_ico(data: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    // Simple ICO parser - get the largest image
    if data.len() < 6 {
        anyhow::bail!("ICO too small");
    }

    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    if count == 0 {
        anyhow::bail!("No images in ICO");
    }

    // Find the largest image
    let mut best_idx = 0;
    let mut best_size = 0u32;

    for i in 0..count {
        let offset = 6 + i * 16;
        if offset + 16 > data.len() {
            break;
        }

        let width = if data[offset] == 0 { 256 } else { data[offset] as u32 };
        let height = if data[offset + 1] == 0 { 256 } else { data[offset + 1] as u32 };
        let size = width * height;

        if size > best_size {
            best_size = size;
            best_idx = i;
        }
    }

    let entry_offset = 6 + best_idx * 16;
    let img_size = u32::from_le_bytes([
        data[entry_offset + 8],
        data[entry_offset + 9],
        data[entry_offset + 10],
        data[entry_offset + 11],
    ]) as usize;
    let img_offset = u32::from_le_bytes([
        data[entry_offset + 12],
        data[entry_offset + 13],
        data[entry_offset + 14],
        data[entry_offset + 15],
    ]) as usize;

    if img_offset + img_size > data.len() {
        anyhow::bail!("Invalid ICO image offset");
    }

    let img_data = &data[img_offset..img_offset + img_size];

    // Check if it's PNG (modern ICO) or BMP
    if img_data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        // PNG - decode it
        decode_png(img_data)
    } else {
        // BMP - parse DIB header
        decode_bmp_dib(img_data)
    }
}

#[cfg(windows)]
fn decode_png(data: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    // Minimal PNG decoder for RGBA icons
    use std::io::Read;
    
    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder.read_info()
        .map_err(|e| anyhow::anyhow!("PNG decode error: {}", e))?;
    
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)
        .map_err(|e| anyhow::anyhow!("PNG frame error: {}", e))?;
    
    buf.truncate(info.buffer_size());
    
    // Convert to RGBA if needed
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(buf.len() / 3 * 4);
            for chunk in buf.chunks(3) {
                rgba.extend_from_slice(chunk);
                rgba.push(255);
            }
            rgba
        }
        _ => anyhow::bail!("Unsupported PNG color type: {:?}", info.color_type),
    };
    
    Ok((rgba, info.width, info.height))
}

#[cfg(windows)]
fn decode_bmp_dib(data: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    // DIB header in ICO
    if data.len() < 40 {
        anyhow::bail!("DIB header too small");
    }

    let width = i32::from_le_bytes([data[4], data[5], data[6], data[7]]) as u32;
    let height = i32::from_le_bytes([data[8], data[9], data[10], data[11]]).abs() as u32 / 2; // ICO stores double height
    let bpp = u16::from_le_bytes([data[14], data[15]]);

    if bpp != 32 {
        anyhow::bail!("Only 32-bit ICO supported, got {}", bpp);
    }

    let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let pixel_data = &data[header_size..];

    let row_size = (width * 4) as usize;
    let expected = row_size * height as usize;

    if pixel_data.len() < expected {
        anyhow::bail!("Not enough pixel data");
    }

    // BMP is bottom-up, BGRA -> RGBA
    let mut rgba = vec![0u8; expected];
    for y in 0..height as usize {
        let src_row = (height as usize - 1 - y) * row_size;
        let dst_row = y * row_size;
        for x in 0..width as usize {
            let src = src_row + x * 4;
            let dst = dst_row + x * 4;
            rgba[dst] = pixel_data[src + 2];     // R
            rgba[dst + 1] = pixel_data[src + 1]; // G
            rgba[dst + 2] = pixel_data[src];     // B
            rgba[dst + 3] = pixel_data[src + 3]; // A
        }
    }

    Ok((rgba, width, height))
}

#[cfg(windows)]
fn open_config_file(path: &std::path::PathBuf) {
    use std::process::Command;
    
    if let Err(e) = Command::new("cmd")
        .args(["/C", "start", "", path.to_str().unwrap_or("")])
        .spawn()
    {
        error!("Failed to open config: {}", e);
    }
}

// Stub for non-Windows
#[cfg(not(windows))]
pub fn run_tray(_shutdown_tx: tokio::sync::broadcast::Sender<()>, _config_path: std::path::PathBuf) {
    tracing::debug!("Tray icon not supported on this platform");
}
