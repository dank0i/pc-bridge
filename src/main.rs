//! PC Agent - Home Assistant integration for Windows
//!
//! Provides:
//! - Game detection via process monitoring
//! - Idle time tracking
//! - Power event handling (sleep/wake)
//! - Remote command execution
//! - MQTT-based communication with Home Assistant

mod config;
mod mqtt;
mod sensors;
mod commands;
mod power;
mod winapi;

use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, error, Level};
use tracing_subscriber::FmtSubscriber;

use crate::config::Config;
use crate::mqtt::{MqttClient, CommandReceiver};
use crate::sensors::{GameSensor, IdleSensor};
use crate::power::PowerEventListener;
use crate::commands::CommandExecutor;

/// Application state shared across tasks
pub struct AppState {
    pub config: RwLock<Config>,
    pub mqtt: MqttClient,
    pub shutdown_tx: broadcast::Sender<()>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    info!("PC Agent starting...");

    // Kill any existing instances
    kill_existing_instances();

    // Load configuration
    let config = Config::load()?;
    info!("Loaded config for device: {}", config.device_name);

    // Create shutdown channel
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Create MQTT client
    let (mqtt, command_rx) = MqttClient::new(&config).await?;

    // Create shared state
    let state = Arc::new(AppState {
        config: RwLock::new(config),
        mqtt,
        shutdown_tx: shutdown_tx.clone(),
    });

    // Start subsystems
    let game_sensor = GameSensor::new(Arc::clone(&state));
    let idle_sensor = IdleSensor::new(Arc::clone(&state));
    let power_listener = PowerEventListener::new(Arc::clone(&state));
    let command_executor = CommandExecutor::new(Arc::clone(&state), command_rx);

    // Spawn sensor tasks
    let game_handle = tokio::spawn(game_sensor.run());
    let idle_handle = tokio::spawn(idle_sensor.run());
    let power_handle = tokio::spawn(power_listener.run());
    let command_handle = tokio::spawn(command_executor.run());

    // Start config file watcher for hot-reload
    let config_watcher_handle = tokio::spawn(config::watch_config(Arc::clone(&state)));

    // Publish initial state
    {
        let config = state.config.read().await;
        state.mqtt.publish_availability(true).await;
        state.mqtt.publish_sensor("sleep_state", "awake").await;
    }

    // Wait for shutdown signal (Ctrl+C)
    info!("PC Agent running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;

    info!("Shutting down...");
    let _ = shutdown_tx.send(());

    // Wait for tasks to finish (with timeout)
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            let _ = game_handle.await;
            let _ = idle_handle.await;
            let _ = power_handle.await;
            let _ = command_handle.await;
            let _ = config_watcher_handle.await;
        }
    ).await;

    // Publish offline status
    state.mqtt.publish_availability(false).await;

    info!("PC Agent stopped");
    Ok(())
}

/// Kill any other running pc-agent.exe processes
fn kill_existing_instances() {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::Threading::*;
    use windows::Win32::Foundation::*;

    let my_pid = std::process::id();
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase().to_string()))
        .unwrap_or_default();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_err() {
            return;
        }
        let snapshot = snapshot.unwrap();

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let proc_name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_lowercase();

                if proc_name == exe_name && entry.th32ProcessID != my_pid {
                    if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                        info!("Killing existing instance (PID {})", entry.th32ProcessID);
                        let _ = TerminateProcess(handle, 0);
                        let _ = CloseHandle(handle);
                    }
                }

                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    // Give processes time to exit
    std::thread::sleep(std::time::Duration::from_millis(200));
}
