//! Cross-platform system tray icon with menu

use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing::{debug, error, info};

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

// Icon embedded in binary (ICO for Windows, will decode PNG from it)
const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.ico");

/// Run the tray icon on a dedicated thread (blocking, uses platform message loop)
pub fn run_tray(shutdown_tx: broadcast::Sender<()>, config_path: PathBuf) {
    info!("Starting system tray");

    // Build menu
    let menu = Menu::new();
    let open_config = MenuItem::new("Open configuration", true, None);
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
    std::thread::spawn(move || loop {
        if let Ok(event) = MenuEvent::receiver().recv() {
            if event.id == open_config_id {
                debug!("Tray: Open configuration clicked");
                open_config_file(&config_path);
            } else if event.id == exit_id {
                info!("Tray: Exit clicked");
                let _ = shutdown_tx_clone.send(());
                break;
            }
        }
    });

    // Platform-specific message loop
    run_message_loop(&shutdown_tx);

    debug!("Tray message loop ended");
}

/// Windows message loop
#[cfg(windows)]
fn run_message_loop(shutdown_tx: &broadcast::Sender<()>) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };

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
}

/// Linux/Unix message loop (GTK-based via tray-icon)
#[cfg(unix)]
fn run_message_loop(shutdown_tx: &broadcast::Sender<()>) {
    // On Linux, tray-icon uses GTK which requires a main loop
    // For now, just sleep and check for shutdown
    use std::time::Duration;

    loop {
        std::thread::sleep(Duration::from_millis(100));
        if shutdown_tx.receiver_count() == 0 {
            break;
        }
    }
}

fn load_icon() -> anyhow::Result<Icon> {
    let (icon_rgba, icon_width, icon_height) = decode_ico(ICON_BYTES)?;
    Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .map_err(|e| anyhow::anyhow!("Failed to create icon: {}", e))
}

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

        let width = if data[offset] == 0 {
            256
        } else {
            data[offset] as u32
        };
        let height = if data[offset + 1] == 0 {
            256
        } else {
            data[offset + 1] as u32
        };
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
        decode_png(img_data)
    } else {
        decode_bmp_dib(img_data)
    }
}

fn decode_png(data: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder
        .read_info()
        .map_err(|e| anyhow::anyhow!("PNG decode error: {}", e))?;

    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
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

fn decode_bmp_dib(data: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    if data.len() < 40 {
        anyhow::bail!("DIB header too small");
    }

    let width = i32::from_le_bytes([data[4], data[5], data[6], data[7]]) as u32;
    let height = i32::from_le_bytes([data[8], data[9], data[10], data[11]]).unsigned_abs() / 2;
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
            rgba[dst] = pixel_data[src + 2]; // R
            rgba[dst + 1] = pixel_data[src + 1]; // G
            rgba[dst + 2] = pixel_data[src]; // B
            rgba[dst + 3] = pixel_data[src + 3]; // A
        }
    }

    Ok((rgba, width, height))
}

/// Open config file with default editor
#[cfg(windows)]
fn open_config_file(path: &PathBuf) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let path_wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let operation: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Open config file with default editor (Linux/macOS)
#[cfg(unix)]
fn open_config_file(path: &PathBuf) {
    use std::process::Command;

    // Try xdg-open (Linux) then open (macOS)
    let result = Command::new("xdg-open").arg(path).spawn();

    if result.is_err() {
        let _ = Command::new("open").arg(path).spawn();
    }
}
