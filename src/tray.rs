//! System tray icon (Windows). A hidden message-only window on a dedicated thread
//! owns a Shell_NotifyIcon tray entry with a right-click menu (Open Settings /
//! Quit) and a double-click-to-open shortcut. Started/stopped live by the manager
//! below as the `show_tray_icon` config flag changes, so it's fully toggleable.
//!
//! Mirrors the hidden-window + message-pump idiom used by the session/power
//! sensors, so it needs no extra crate.
#![cfg(windows)]

use std::sync::Arc;

use log::{debug, error, info};
use tokio::sync::broadcast;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, HICON,
    IDI_APPLICATION, LoadIconW, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_RBUTTONUP, WM_USER, WNDCLASSEXW,
};

use crate::AppState;

/// Message the tray icon posts to our window on mouse events.
const WM_TRAYICON: u32 = WM_APP + 1;
/// Menu command ids.
const ID_OPEN: usize = 1;
const ID_QUIT: usize = 2;
/// Our single tray icon's id within the window.
const TRAY_UID: u32 = 1;

/// Per-window state handed to the wnd_proc via GWLP_USERDATA.
struct TrayContext {
    shutdown_tx: broadcast::Sender<()>,
}

/// Async manager: create/destroy the tray as `show_tray_icon` changes, and tear it
/// down on global shutdown. Spawned once (Windows only) from the agent.
pub async fn run_manager(state: Arc<AppState>) {
    let mut shutdown_rx = state.shutdown_tx.subscribe();
    let mut config_rx = state.config_generation.subscribe();
    let mut current: Option<isize> = None; // HWND of the running tray, if any

    loop {
        let want = state.config.read().await.show_tray_icon;
        match (want, current) {
            (true, None) => current = spawn_tray(state.shutdown_tx.clone()).await,
            (false, Some(hwnd)) => {
                stop_tray(hwnd);
                current = None;
            }
            _ => {}
        }

        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => {
                if let Some(hwnd) = current {
                    stop_tray(hwnd);
                }
                break;
            }
            r = config_rx.recv() => {
                if !matches!(r, Ok(()) | Err(broadcast::error::RecvError::Lagged(_))) {
                    break;
                }
            }
        }
    }
}

/// Spawn the tray thread and return its window handle once created.
async fn spawn_tray(shutdown_tx: broadcast::Sender<()>) -> Option<isize> {
    let (hwnd_tx, hwnd_rx) = tokio::sync::oneshot::channel::<isize>();
    if let Err(e) = std::thread::Builder::new()
        .name("tray".into())
        .stack_size(256 * 1024)
        .spawn(move || tray_thread(&shutdown_tx, hwnd_tx))
    {
        error!("Failed to spawn tray thread: {e}");
        return None;
    }
    hwnd_rx.await.ok()
}

/// Ask the tray thread to remove its icon and exit (unblocks its GetMessage pump).
fn stop_tray(hwnd: isize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_USER, WPARAM(0), LPARAM(0));
    }
}

fn tray_thread(shutdown_tx: &broadcast::Sender<()>, hwnd_tx: tokio::sync::oneshot::Sender<isize>) {
    unsafe {
        let class_name = windows::core::w!("PCAgentTray");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&raw const wc);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            windows::core::w!("pc-bridge tray"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                error!("Failed to create tray window: {e:?}");
                return;
            }
        };

        // Stash the shutdown sender for the wnd_proc.
        let ctx = Box::new(TrayContext {
            shutdown_tx: shutdown_tx.clone(),
        });
        let ctx_ptr = Box::into_raw(ctx);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr as isize);

        // Add the tray icon.
        let hicon: HICON = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: hicon,
            ..Default::default()
        };
        // Tooltip text (UTF-16, NUL-terminated within the fixed buffer).
        for (dst, ch) in nid.szTip.iter_mut().zip("pc-bridge".encode_utf16()) {
            *dst = ch;
        }
        if !Shell_NotifyIconW(NIM_ADD, &raw const nid).as_bool() {
            error!("Shell_NotifyIcon(NIM_ADD) failed");
            let _ = Box::from_raw(ctx_ptr);
            let _ = DestroyWindow(hwnd);
            return;
        }
        info!("Tray icon added");

        let _ = hwnd_tx.send(hwnd.0 as isize);

        // Pump messages until we're asked to stop (WM_USER) or the window is gone.
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&raw mut msg, None, 0, 0);
            if !ret.as_bool() || ret.0 == -1 {
                break;
            }
            if msg.message == WM_USER {
                break;
            }
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }

        // Remove the icon and clean up.
        let _ = Shell_NotifyIconW(NIM_DELETE, &raw const nid);
        let _ = Box::from_raw(ctx_ptr);
        let _ = DestroyWindow(hwnd);
        debug!("Tray thread exiting");
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_TRAYICON {
            // lParam low word is the triggering mouse message.
            let event = (lparam.0 as u32) & 0xFFFF;
            match event {
                WM_LBUTTONDBLCLK => open_settings(),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            return LRESULT(0);
        }
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Show the right-click context menu and act on the selection.
unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, windows::core::w!("Open Settings"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            ID_QUIT,
            windows::core::w!("Quit pc-bridge"),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&raw mut pt);
        // Required so the menu dismisses correctly when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), 0, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            ID_OPEN => open_settings(),
            ID_QUIT => {
                if let Some(ctx) = context(hwnd) {
                    info!("Tray: Quit selected");
                    let _ = ctx.shutdown_tx.send(());
                }
                // Also stop this pump; the agent will exit on the shutdown signal.
                PostQuitMessage(0);
            }
            _ => {}
        }
    }
}

/// Launch a separate `--ui` settings-window process.
fn open_settings() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--ui").spawn();
    }
}

unsafe fn context(hwnd: HWND) -> Option<&'static TrayContext> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayContext;
        ptr.as_ref()
    }
}
