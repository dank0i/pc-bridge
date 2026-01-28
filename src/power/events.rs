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
const PBT_POWERSETTINGCHANGE: usize = 0x8013;

// GUID for monitor power setting  
const GUID_CONSOLE_DISPLAY_STATE: windows::core::GUID = windows::core::GUID::from_u128(
    0x6fe69556_704a_47a0_8f24_c28d936fda47
);

#[derive(Debug, Clone, Copy)]
pub enum PowerEvent {
    Sleep,
    Wake,
}

// Track whether we've seen a sleep event (to avoid wake on startup)
static HAS_SLEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            return;
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

            // Create a real window (not HWND_MESSAGE) to receive power broadcasts
            // HWND_MESSAGE windows don't receive broadcast messages!
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Power Monitor"),
                WINDOW_STYLE::default(),
                0, 0, 0, 0,
                None,  // No parent - creates top-level window that can receive broadcasts
                None,
                None,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    error!("Failed to create power monitor window: {:?}", e);
                    return;
                }
            };

            // Store event_tx in window's user data using Box to ensure stable address
            let event_tx_box = Box::new(event_tx);
            let event_tx_ptr = Box::into_raw(event_tx_box);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, event_tx_ptr as isize);

            // Register for power setting notifications (modern API, more reliable)
            let _power_notify = RegisterPowerSettingNotification(
                hwnd,
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            );
            
            info!("Power event listener started (hwnd: {:?})", hwnd);

            // Message loop
            let mut msg = MSG::default();
            loop {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                
                // Use PeekMessage with timeout to allow checking stopped flag
                let ret = PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE);
                if ret.as_bool() {
                    if msg.message == WM_QUIT {
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    // No message, sleep briefly
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            // Cleanup
            let _ = Box::from_raw(event_tx_ptr); // Reclaim the Box to drop it
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
                
                debug!("WM_POWERBROADCAST: wparam={}", wparam.0);
                
                match wparam.0 {
                    PBT_APMSUSPEND => {
                        info!("Received PBT_APMSUSPEND");
                        HAS_SLEPT.store(true, Ordering::SeqCst);
                        let _ = event_tx.blocking_send(PowerEvent::Sleep);
                    }
                    PBT_APMRESUMEAUTO | PBT_APMRESUMESUSPEND => {
                        info!("Received PBT_APMRESUME*");
                        // These are real system resume events, always trigger wake
                        HAS_SLEPT.store(true, Ordering::SeqCst); // Ensure future display-on events work
                        let _ = event_tx.blocking_send(PowerEvent::Wake);
                    }
                    PBT_POWERSETTINGCHANGE => {
                        // Handle power setting change (from RegisterPowerSettingNotification)
                        let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
                        if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
                            let state = setting.Data[0];
                            debug!("Display state change: {}", state);
                            // 0 = off (sleep), 1 = on, 2 = dimmed
                            if state == 0 {
                                HAS_SLEPT.store(true, Ordering::SeqCst);
                                let _ = event_tx.blocking_send(PowerEvent::Sleep);
                            } else if state == 1 && HAS_SLEPT.load(Ordering::SeqCst) {
                                // Only trigger wake if we've previously slept
                                let _ = event_tx.blocking_send(PowerEvent::Wake);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
