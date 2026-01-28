//! Power event listener - detects sleep/wake events

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tracing::{info, debug, error};
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::Power::*;

use crate::AppState;
use super::display::wake_display_with_retry;

const WM_POWERBROADCAST: u32 = 0x218;
const PBT_APMSUSPEND: usize = 4;
const PBT_APMRESUMEAUTO: usize = 0x12;
const PBT_APMRESUMESUSPEND: usize = 7;

#[derive(Debug, Clone, Copy)]
pub enum PowerEvent {
    Sleep,
    Wake,
}

pub struct PowerEventListener {
    state: Arc<AppState>,
}

impl PowerEventListener {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let (event_tx, mut event_rx) = mpsc::channel::<PowerEvent>(10);
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Spawn blocking thread for Windows message pump
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = Arc::clone(&stopped);
        
        std::thread::spawn(move || {
            Self::message_pump(event_tx, stopped_clone);
        });

        // Handle events
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Power listener shutting down");
                    stopped.store(true, Ordering::SeqCst);
                    break;
                }
                Some(event) = event_rx.recv() => {
                    match event {
                        PowerEvent::Sleep => {
                            info!("Power event: SLEEP");
                            self.state.mqtt.publish_sensor_retained("sleep_state", "sleeping").await;
                        }
                        PowerEvent::Wake => {
                            info!("Power event: WAKE");
                            // Wake display first
                            wake_display_with_retry(3, std::time::Duration::from_millis(500));
                            
                            // Try to publish wake state with retries (network may take time)
                            self.publish_wake_with_retry().await;
                        }
                    }
                }
            }
        }
    }

    async fn publish_wake_with_retry(&self) {
        let delays = [
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(10),
        ];

        for delay in delays {
            tokio::time::sleep(delay).await;
            self.state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
            info!("Published awake state");
            return; // TODO: Check if actually connected before returning
        }
    }

    fn message_pump(event_tx: mpsc::Sender<PowerEvent>, stopped: Arc<AtomicBool>) {
        unsafe {
            // Register window class
            let class_name = windows::core::w!("PCAgentPowerMonitor");
            
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(Self::wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };

            RegisterClassExW(&wc);

            // Create message-only window
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!(""),
                WINDOW_STYLE::default(),
                0, 0, 0, 0,
                HWND_MESSAGE,
                None,
                None,
                Some(&event_tx as *const _ as *const std::ffi::c_void),
            ) {
                Ok(h) => h,
                Err(e) => {
                    error!("Failed to create power monitor window: {:?}", e);
                    return;
                }
            };

            // Store event_tx in window's user data
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &event_tx as *const _ as isize);

            info!("Power event listener started");

            // Message loop
            let mut msg = MSG::default();
            while !stopped.load(Ordering::SeqCst) {
                let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
                if ret.0 <= 0 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let _ = DestroyWindow(hwnd);
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_POWERBROADCAST {
            let event_tx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const mpsc::Sender<PowerEvent>;
            
            if !event_tx_ptr.is_null() {
                let event_tx = &*event_tx_ptr;
                
                match wparam.0 {
                    PBT_APMSUSPEND => {
                        let _ = event_tx.blocking_send(PowerEvent::Sleep);
                    }
                    PBT_APMRESUMEAUTO | PBT_APMRESUMESUSPEND => {
                        let _ = event_tx.blocking_send(PowerEvent::Wake);
                    }
                    _ => {}
                }
            }
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
