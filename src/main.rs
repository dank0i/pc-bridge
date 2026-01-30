//! PC Bridge - Home Assistant integration for Windows and Linux
//!
//! Provides:
//! - Game detection via process monitoring
//! - Idle time tracking
//! - Power event handling (sleep/wake)
//! - Remote command execution
//! - MQTT-based communication with Home Assistant

#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod mqtt;
mod sensors;
mod commands;
mod power;
mod updater;
mod tray;
mod notification;

#[cfg(windows)]
mod winapi;

use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::config::Config;
use crate::mqtt::MqttClient;
use crate::sensors::{GameSensor, IdleSensor, MemorySensor, CustomSensorManager};
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
    let _subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    info!("PC Agent starting...");

    // Kill any existing instances
    kill_existing_instances();

    // Check for updates (non-blocking, continues after check)
    tokio::spawn(updater::check_for_updates());

    // Load configuration
    let config = Config::load()?;
    info!("Loaded config for device: {}", config.device_name);
    let show_tray = config.show_tray_icon;
    let config_path = Config::config_path()?;

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
    let memory_sensor = MemorySensor::new(Arc::clone(&state));
    let power_listener = PowerEventListener::new(Arc::clone(&state));
    let command_executor = CommandExecutor::new(Arc::clone(&state), command_rx);
    let custom_sensor_manager = CustomSensorManager::new(Arc::clone(&state));

    // Spawn sensor tasks
    let game_handle = tokio::spawn(game_sensor.run());
    let idle_handle = tokio::spawn(idle_sensor.run());
    let memory_handle = tokio::spawn(memory_sensor.run());
    let power_handle = tokio::spawn(power_listener.run());
    let command_handle = tokio::spawn(command_executor.run());
    let custom_sensor_handle = tokio::spawn(custom_sensor_manager.run());

    // Start config file watcher for hot-reload
    let config_watcher_handle = tokio::spawn(config::watch_config(Arc::clone(&state)));

    // Start tray icon (Windows only, runs on separate thread)
    if show_tray {
        let tray_shutdown = shutdown_tx.clone();
        let tray_config_path = config_path.clone();
        std::thread::spawn(move || {
            tray::run_tray(tray_shutdown, tray_config_path);
        });
    }

    // Publish initial state and register custom entities
    {
        let config = state.config.read().await;
        state.mqtt.publish_availability(true).await;
        state.mqtt.publish_sensor_retained("sleep_state", "awake").await;
        
        // Register custom sensors if enabled
        if config.custom_sensors_enabled && !config.custom_sensors.is_empty() {
            state.mqtt.register_custom_sensors(&config.custom_sensors).await;
        }
        
        // Register custom commands if enabled
        if config.custom_commands_enabled && !config.custom_commands.is_empty() {
            state.mqtt.register_custom_commands(&config.custom_commands).await;
        }
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
            let _ = memory_handle.await;
            let _ = power_handle.await;
            let _ = command_handle.await;
            let _ = custom_sensor_handle.await;
            let _ = config_watcher_handle.await;
        }
    ).await;

    // Publish offline status
    state.mqtt.publish_availability(false).await;

    info!("PC Agent stopped");
    Ok(())
}

/// Kill any other running instances (platform-specific)
#[cfg(windows)]
fn kill_existing_instances() {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::Threading::*;
    use windows::Win32::Foundation::*;

    let my_pid = std::process::id();
    
    // Match any of these exe names (covers renames)
    let exe_names = ["pc-bridge.exe", "pc bridge.exe", "pc-agent.exe"];

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(e) => {
                info!("Failed to create process snapshot: {:?}", e);
                return;
            }
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let proc_name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_lowercase();

                // Check if this process matches any of our exe names
                let is_match = exe_names.iter().any(|&name| proc_name == name);
                
                if is_match && entry.th32ProcessID != my_pid {
                    if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                        info!("Killing existing instance: {} (PID {})", proc_name, entry.th32ProcessID);
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
    std::thread::sleep(std::time::Duration::from_millis(500));
}

/// Kill any other running instances (Linux)
#[cfg(unix)]
fn kill_existing_instances() {
    use std::process::Command;
    
    let my_pid = std::process::id();
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();
    
    if exe_name.is_empty() {
        return;
    }
    
    // Use pkill to kill other instances, excluding our PID
    // This is a safe approach - pkill won't kill itself
    let _ = Command::new("pkill")
        .args(["-f", &exe_name, "--signal", "TERM"])
        .spawn();
    
    std::thread::sleep(std::time::Duration::from_millis(200));
}
