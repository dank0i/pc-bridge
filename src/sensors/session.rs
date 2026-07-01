//! Session lock/unlock sensor (Windows).
//!
//! Detects workstation lock and unlock via WTS session notifications and
//! publishes "locked"/"unlocked" to the `session` sensor. Uses its own hidden
//! message-pump window so it is fully isolated from the power-events listener
//! (which handles sleep/wake) - a bug here can never affect sleep detection.

use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::mpsc;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, MSG, PostMessageW, RegisterClassExW, SetWindowLongPtrW, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_USER, WNDCLASSEXW,
};

use crate::AppState;

const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;
const NOTIFY_FOR_THIS_SESSION: u32 = 0;

/// Lock/unlock event carried from the message pump to the async publisher.
#[derive(Debug, Clone, Copy)]
enum SessionEvent {
    Locked,
    Unlocked,
}

/// Stored in the window's user data so `wnd_proc` can forward events.
struct WndProcContext {
    event_tx: mpsc::Sender<SessionEvent>,
}

pub struct SessionSensor {
    state: Arc<AppState>,
}

impl SessionSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
        let (event_tx, mut event_rx) = mpsc::channel::<SessionEvent>(10);
        let mut shutdown_rx = shutdown.subscribe();

        // Spawn the blocking message pump and recover its hwnd so we can post a
        // quit message on shutdown.
        let (hwnd_tx, hwnd_rx) = tokio::sync::oneshot::channel::<isize>();
        match std::thread::Builder::new()
            .name("session-events".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                Self::message_pump(event_tx, hwnd_tx);
            }) {
            Ok(_) => {}
            Err(e) => {
                error!("Failed to spawn session events thread: {}", e);
                return;
            }
        }

        let pump_hwnd = hwnd_rx.await.ok();

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Session listener shutting down");
                    if let Some(hwnd_val) = pump_hwnd {
                        unsafe {
                            let hwnd = HWND(hwnd_val as *mut _);
                            let _ = PostMessageW(hwnd, WM_USER, WPARAM(0), LPARAM(0));
                        }
                    }
                    break;
                }
                Some(event) = event_rx.recv() => {
                    let value = match event {
                        SessionEvent::Locked => "locked",
                        SessionEvent::Unlocked => "unlocked",
                    };
                    info!("Session event: {}", value);
                    self.state
                        .mqtt
                        .publish_sensor_retained("session", value)
                        .await;
                }
            }
        }
    }

    fn message_pump(
        event_tx: mpsc::Sender<SessionEvent>,
        hwnd_tx: tokio::sync::oneshot::Sender<isize>,
    ) {
        unsafe {
            let class_name = windows::core::w!("PCAgentSessionMonitor");

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(Self::wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassExW(&raw const wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Session Monitor"),
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
                    error!("Failed to create session monitor window: {:?}", e);
                    return;
                }
            };

            if let Err(e) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
                error!("Failed to register session notifications: {:?}", e);
            } else {
                info!("Registered for session lock/unlock notifications");
            }

            let ctx = Box::new(WndProcContext { event_tx });
            let ctx_ptr = Box::into_raw(ctx);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr as isize);

            let _ = hwnd_tx.send(hwnd.0 as isize);

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

            let _ = WTSUnRegisterSessionNotification(hwnd);
            let _ = Box::from_raw(ctx_ptr);
            let _ = DestroyWindow(hwnd);
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if msg == WM_WTSSESSION_CHANGE {
                let ctx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndProcContext;
                if !ctx_ptr.is_null() {
                    let ctx = &*ctx_ptr;
                    match wparam.0 {
                        WTS_SESSION_LOCK => {
                            let _ = ctx.event_tx.blocking_send(SessionEvent::Locked);
                        }
                        WTS_SESSION_UNLOCK => {
                            let _ = ctx.event_tx.blocking_send(SessionEvent::Unlocked);
                        }
                        _ => {}
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
