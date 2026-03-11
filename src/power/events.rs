//! Power event listener - detects sleep/wake and display power events
//!
//! Uses atomic state machine to ensure exactly one sleep and one wake event
//! per actual power state transition, regardless of how many Windows messages arrive.
//!
//! Also monitors display power state via GUID_CONSOLE_DISPLAY_STATE to detect
//! when Windows turns off the monitor (separate from screensaver).
//!
//! Sleep event publishing uses a **synchronous TCP connection** from the
//! power-events thread to guarantee the MQTT PUBLISH packet reaches the
//! broker before `wnd_proc` returns. The async event loop cannot provide
//! this guarantee because Windows may suspend the NIC before the tokio
//! runtime flushes the message to TCP.

use log::{debug, error, info, warn};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Power::RegisterPowerSettingNotification;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, MSG, PostMessageW, RegisterClassExW,
    SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_USER, WNDCLASSEXW,
};

use super::display::wake_display_with_retry;
use super::sync_mqtt::{SyncMqttConfig, parse_broker_url, sync_mqtt_publish_sleep};
use crate::AppState;

const WM_POWERBROADCAST: u32 = 0x218;
const PBT_APMSUSPEND: usize = 4;
const PBT_APMRESUMEAUTO: usize = 0x12;
const PBT_APMRESUMESUSPEND: usize = 7;
const PBT_POWERSETTINGCHANGE: usize = 0x8013;

/// GUID_CONSOLE_DISPLAY_STATE: {6FE69556-704A-47A0-8F24-C28D936FDA47}
/// Data values: 0 = off, 1 = on, 2 = dimmed
const GUID_CONSOLE_DISPLAY_STATE: windows::core::GUID = windows::core::GUID::from_values(
    0x6FE6_9556,
    0x704A,
    0x47A0,
    [0x8F, 0x24, 0xC2, 0x8D, 0x93, 0x6F, 0xDA, 0x47],
);

/// Layout of the POWERBROADCAST_SETTING structure from WM_POWERBROADCAST/PBT_POWERSETTINGCHANGE
#[repr(C)]
struct PowerBroadcastSetting {
    power_setting: windows::core::GUID,
    data_length: u32,
    data: [u8; 1],
}

#[derive(Debug)]
pub enum PowerEvent {
    Sleep,
    Wake,
    DisplayOff,
    DisplayOn,
}

/// Context stored in the power-monitor window's user data.
struct WndProcContext {
    event_tx: mpsc::Sender<PowerEvent>,
    sync_mqtt: SyncMqttConfig,
}

// State machine: 0 = awake, 1 = sleeping
// Using compare_exchange ensures only the FIRST event of a type triggers an action
static POWER_STATE: AtomicU8 = AtomicU8::new(0); // Start awake

/// Attempt to transition from awake (0) to sleeping (1).
/// Returns true only if this call performed the transition.
fn try_transition_to_sleep() -> bool {
    POWER_STATE
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Attempt to transition from sleeping (1) to awake (0).
/// Returns true only if this call performed the transition.
fn try_transition_to_awake() -> bool {
    POWER_STATE
        .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
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

        // Build sync MQTT config for the power-events thread.
        // This lets wnd_proc publish the sleep message over a dedicated TCP
        // connection, independent of the async event loop.
        let sync_mqtt = {
            let config = self.state.config.read().await;
            let broker = &config.mqtt.broker;
            let (host, port) = parse_broker_url(broker);
            SyncMqttConfig {
                host,
                port,
                user: config.mqtt.user.clone(),
                pass: config.mqtt.pass.clone(),
                // Use a distinct client_id so the broker doesn't kick our main connection
                client_id: format!("{}-sleep", config.client_id()),
                sleep_topic: format!(
                    "homeassistant/sensor/{}/sleep_state/state",
                    config.device_name
                ),
            }
        };

        // Spawn blocking thread for Windows message pump
        // Store hwnd so we can post WM_QUIT on shutdown
        let (hwnd_tx, hwnd_rx) = tokio::sync::oneshot::channel::<isize>();

        match std::thread::Builder::new()
            .name("power-events".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                Self::message_pump(event_tx, sync_mqtt, hwnd_tx);
            }) {
            Ok(_) => {}
            Err(e) => {
                error!("Failed to spawn power events thread: {}", e);
                return;
            }
        }

        // Wait for hwnd from the message pump thread
        let pump_hwnd = hwnd_rx.await.ok();

        // Handle events (no debouncing needed - state machine handles deduplication)
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Power listener shutting down");
                    // Post WM_QUIT to unblock GetMessageW
                    if let Some(hwnd_val) = pump_hwnd {
                        unsafe {
                            let hwnd = HWND(hwnd_val as *mut _);
                            let _ = PostMessageW(hwnd, WM_USER, WPARAM(0), LPARAM(0));
                        }
                    }
                    break;
                }
                Some(event) = event_rx.recv() => {
                    match event {
                        PowerEvent::Sleep => {
                            info!("Power event: SLEEP (async fallback — sync TCP already attempted in wnd_proc)");
                            // Fallback publish via the async client. Harmless if the
                            // sync TCP publish already landed (retained = last-write-wins).
                            // Catches cases where sync fails (TLS broker, Modern Standby, etc.).
                            self.state.mqtt.publish_sensor_retained("sleep_state", "sleeping").await;
                        }
                        PowerEvent::Wake => {
                            info!("Power event: WAKE");
                            // Wake display on blocking thread to avoid stalling async runtime
                            tokio::task::spawn_blocking(|| {
                                wake_display_with_retry(3, std::time::Duration::from_millis(500));
                            });

                            // Publish wake state with retries in background task
                            // so the event handler stays responsive to new events
                            let mqtt = &self.state.mqtt;
                            mqtt.publish_sensor_retained("sleep_state", "awake").await;
                            info!("Published awake state");
                            let state = Arc::clone(&self.state);
                            tokio::spawn(async move {
                                for delay_secs in [2, 5, 10] {
                                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                                    state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
                                }
                            });
                        }
                        PowerEvent::DisplayOff => {
                            info!("Power event: DISPLAY OFF");
                            self.state.mqtt.publish_sensor_retained("display", "off").await;
                        }
                        PowerEvent::DisplayOn => {
                            info!("Power event: DISPLAY ON");
                            self.state.mqtt.publish_sensor_retained("display", "on").await;
                        }
                    }
                }
            }
        }
    }

    fn message_pump(
        event_tx: mpsc::Sender<PowerEvent>,
        sync_mqtt: SyncMqttConfig,
        hwnd_tx: tokio::sync::oneshot::Sender<isize>,
    ) {
        unsafe {
            let class_name = windows::core::w!("PCAgentPowerMonitor");

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(Self::wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };

            RegisterClassExW(&raw const wc);

            // Create a real window to receive power broadcasts
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                windows::core::w!("PC Agent Power Monitor"),
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
                    error!("Failed to create power monitor window: {:?}", e);
                    return;
                }
            };

            // Register for display power state notifications
            if let Err(e) = RegisterPowerSettingNotification(
                HANDLE(hwnd.0),
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            ) {
                error!("Failed to register display power notification: {:?}", e);
            } else {
                info!("Registered for display power state notifications");
            }

            // Store context (event_tx + sync mqtt config) in window's user data
            let ctx = Box::new(WndProcContext {
                event_tx,
                sync_mqtt,
            });
            let ctx_ptr = Box::into_raw(ctx);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr as isize);

            info!("Power event listener started (hwnd: {:?})", hwnd);

            // Send hwnd back so async side can post WM_USER to unblock GetMessageW
            let _ = hwnd_tx.send(hwnd.0 as isize);

            // Message loop - blocks on GetMessageW (zero CPU when idle)
            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&raw mut msg, None, 0, 0);
                if !ret.as_bool() || ret.0 == -1 {
                    // WM_QUIT received or error
                    break;
                }
                // WM_USER is our custom shutdown signal
                if msg.message == WM_USER {
                    break;
                }
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }

            // Cleanup
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
            if msg == WM_POWERBROADCAST {
                let ctx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndProcContext;

                if !ctx_ptr.is_null() {
                    let ctx = &*ctx_ptr;

                    match wparam.0 {
                        PBT_APMSUSPEND => {
                            debug!("Received PBT_APMSUSPEND");
                            // Only fire if transitioning from awake to sleeping
                            if try_transition_to_sleep() {
                                info!("State transition: awake -> sleeping");
                                // Synchronous MQTT publish over a dedicated TCP connection.
                                // This blocks wnd_proc until the packet is on the wire,
                                // guaranteeing delivery before Windows suspends the NIC.
                                match sync_mqtt_publish_sleep(&ctx.sync_mqtt) {
                                    Ok(()) => info!("Sleep state published via sync TCP"),
                                    Err(e) => warn!("Sync MQTT publish failed: {}", e),
                                }
                                // Also notify the async handler (redundant publish + logging)
                                let _ = ctx.event_tx.blocking_send(PowerEvent::Sleep);
                            } else {
                                debug!("Ignoring duplicate sleep event");
                            }
                        }
                        PBT_APMRESUMEAUTO | PBT_APMRESUMESUSPEND => {
                            debug!("Received PBT_APMRESUME* (wparam={})", wparam.0);
                            // Only fire if transitioning from sleeping to awake
                            if try_transition_to_awake() {
                                info!("State transition: sleeping -> awake");
                                let _ = ctx.event_tx.blocking_send(PowerEvent::Wake);
                            } else {
                                debug!("Ignoring duplicate wake event");
                            }
                        }
                        PBT_POWERSETTINGCHANGE => {
                            // Display power state change notification
                            let pbs = lparam.0 as *const PowerBroadcastSetting;
                            if !pbs.is_null() {
                                let setting = &*pbs;
                                if setting.power_setting == GUID_CONSOLE_DISPLAY_STATE
                                    && setting.data_length >= 1
                                {
                                    let display_state = setting.data[0];
                                    debug!(
                                        "Display power state change: {}",
                                        match display_state {
                                            0 => "off",
                                            1 => "on",
                                            2 => "dimmed",
                                            _ => "unknown",
                                        }
                                    );
                                    match display_state {
                                        0 => {
                                            let _ =
                                                ctx.event_tx.blocking_send(PowerEvent::DisplayOff);
                                        }
                                        1 => {
                                            let _ =
                                                ctx.event_tx.blocking_send(PowerEvent::DisplayOn);
                                        }
                                        2 => {
                                            // Dimmed - treat as still on (display is visible)
                                            debug!("Display dimmed, treating as on");
                                        }
                                        _ => {
                                            debug!("Unknown display state: {}", display_state);
                                        }
                                    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_machine_transitions() {
        // Reset to known state
        POWER_STATE.store(0, Ordering::SeqCst);

        assert!(try_transition_to_sleep());
        assert!(!try_transition_to_sleep()); // duplicate
        assert!(try_transition_to_awake());
        assert!(!try_transition_to_awake()); // duplicate

        // Reset for other tests
        POWER_STATE.store(0, Ordering::SeqCst);
    }
}
