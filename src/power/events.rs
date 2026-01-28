//! Power event listener - detects sleep/wake events
//!
//! Uses atomic state machine to ensure exactly one sleep and one wake event
//! per actual power state transition, regardless of how many Windows messages arrive.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::sync::mpsc;
use tracing::{info, debug, error};
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::AppState;
use super::display::wake_display_with_retry;

const WM_POWERBROADCAST: u32 = 0x218;
const PBT_APMSUSPEND: usize = 4;
const PBT_APMRESUMEAUTO: usize = 0x12;
const PBT_APMRESUMESUSPEND: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PowerEvent {
    Sleep,
    Wake,
}

// State machine: 0 = awake, 1 = sleeping
// Using compare_exchange ensures only the FIRST event of a type triggers an action
static POWER_STATE: AtomicU8 = AtomicU8::new(0); // Start awake

/// Attempt to transition from awake (0) to sleeping (1).
/// Returns true only if this call performed the transition.
fn try_transition_to_sleep() -> bool {
    POWER_STATE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Attempt to transition from sleeping (1) to awake (0).
/// Returns true only if this call performed the transition.
fn try_transition_to_awake() -> bool {
    POWER_STATE.compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok()
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

        // Handle events (no debouncing needed - state machine handles deduplication)
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
                            
                            // Publish wake state with retries (network may take time)
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
            let class_name = windows::core::w!("PCAgentPowerMonitor");
            
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(Self::wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };

            RegisterClassExW(&wc);

            // Create a real window to receive power broadcasts
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Power Monitor"),
                WINDOW_STYLE::default(),
                0, 0, 0, 0,
                None,
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

            // Store event_tx in window's user data
            let event_tx_box = Box::new(event_tx);
            let event_tx_ptr = Box::into_raw(event_tx_box);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, event_tx_ptr as isize);
            
            info!("Power event listener started (hwnd: {:?})", hwnd);

            // Message loop
            let mut msg = MSG::default();
            loop {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                
                let ret = PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE);
                if ret.as_bool() {
                    if msg.message == WM_QUIT {
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            // Cleanup
            let _ = Box::from_raw(event_tx_ptr);
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
                        debug!("Received PBT_APMSUSPEND");
                        // Only fire if transitioning from awake to sleeping
                        if try_transition_to_sleep() {
                            info!("State transition: awake -> sleeping");
                            let _ = event_tx.blocking_send(PowerEvent::Sleep);
                        } else {
                            debug!("Ignoring duplicate sleep event");
                        }
                    }
                    PBT_APMRESUMEAUTO | PBT_APMRESUMESUSPEND => {
                        debug!("Received PBT_APMRESUME* (wparam={})", wparam.0);
                        // Only fire if transitioning from sleeping to awake
                        if try_transition_to_awake() {
                            info!("State transition: sleeping -> awake");
                            let _ = event_tx.blocking_send(PowerEvent::Wake);
                        } else {
                            debug!("Ignoring duplicate wake event");
                        }
                    }
                    _ => {}
                }
            }
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
