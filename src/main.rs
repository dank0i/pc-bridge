//! PC Bridge - Home Assistant integration for Windows and Linux
//!
//! Provides:
//! - Game detection via process monitoring (auto-discovers Steam games)
//! - Idle time tracking
//! - Power event handling (sleep/wake)
//! - Remote command execution
//! - MQTT-based communication with Home Assistant

#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod commands;
mod config;
mod mqtt;
mod notification;
mod power;
mod sensors;
mod setup;
mod steam;
mod tray;
mod updater;

#[cfg(windows)]
mod winapi;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use crate::commands::CommandExecutor;
use crate::config::Config;
use crate::mqtt::MqttClient;
use crate::power::PowerEventListener;
#[cfg(windows)]
use crate::sensors::ProcessWatcher;
use crate::sensors::{CustomSensorManager, GameSensor, IdleSensor, SystemSensor};
use crate::steam::SteamGameDiscovery;

/// Application state shared across tasks
pub struct AppState {
    pub config: RwLock<Config>,
    pub mqtt: MqttClient,
    pub shutdown_tx: broadcast::Sender<()>,
    /// Notifies subscribers when config is reloaded (hot-reload)
    pub config_generation: broadcast::Sender<()>,
    /// Event-driven process watcher using WMI (Windows only)
    /// Provides always-up-to-date process list for game detection and screensaver
    #[cfg(windows)]
    pub process_watcher: ProcessWatcher,
    /// Display power state: true when Windows has powered off the display
    /// Set by PowerEventListener via GUID_CONSOLE_DISPLAY_STATE notifications
    pub display_off: AtomicBool,
}

/// Handle for optional tasks
type TaskHandle = tokio::task::JoinHandle<()>;

#[cfg(windows)]
static SHUTDOWN_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows::Win32::Foundation::BOOL {
    use windows::Win32::Foundation::BOOL;
    // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1, CTRL_CLOSE_EVENT = 2
    if ctrl_type <= 2 {
        // Just exit - graceful shutdown doesn't work well with GUI subsystem + attached console
        std::process::exit(0);
    }
    BOOL(0) // Not handled
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // On Windows, attach to parent console if launched from terminal
    // This allows seeing output when run from cmd/powershell
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{
            AttachConsole, GetStdHandle, SetConsoleCtrlHandler, SetConsoleMode,
            ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
        };
        unsafe {
            // ATTACH_PARENT_PROCESS = -1 (0xFFFFFFFF)
            if AttachConsole(u32::MAX).is_ok() {
                // Enable Ctrl+C processing on the attached console
                if let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) {
                    let _ = SetConsoleMode(handle, ENABLE_PROCESSED_INPUT);
                }
                // Set up Ctrl+C handler
                let _ = SetConsoleCtrlHandler(Some(console_ctrl_handler), true);
            }
        }
    }

    // Initialize logging
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    info!("PC Bridge starting...");

    // Kill any existing instances
    kill_existing_instances();

    // Check for first run - show setup wizard if no config exists
    if Config::is_first_run()? {
        info!("First run detected - launching setup wizard");

        if let Some(setup_config) = setup::run_setup_wizard() {
            setup::save_setup_config(&setup_config)?;
            info!("Setup complete! Configuration saved.");
        } else {
            error!("Setup cancelled by user");
            #[cfg(windows)]
            {
                use windows::core::w;
                use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
                unsafe {
                    MessageBoxW(
                        None,
                        w!("Setup was cancelled.\n\nPC Bridge will now exit."),
                        w!("PC Bridge"),
                        MB_OK | MB_ICONWARNING,
                    );
                }
            }
            return Ok(());
        }
    }

    // Check for updates (non-blocking, continues after check)
    tokio::spawn(updater::check_for_updates());

    // Load configuration
    let config = Config::load()?;
    info!("Loaded config for device: {}", config.device_name);

    // Log enabled features
    log_enabled_features(&config);

    let show_tray = config.features.show_tray_icon;
    let config_path = Config::config_path()?;

    // Create shutdown channel
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Create MQTT client (conditionally registers discovery based on features)
    let (mqtt, command_rx) = MqttClient::new(&config).await?;

    // Discover Steam games and merge into config if game detection is enabled
    let mut config = config;
    if config.features.game_detection {
        info!("Discovering Steam games...");
        if let Some(discovery) = SteamGameDiscovery::discover_async().await {
            info!(
                "  Found {} Steam games in {}ms{}",
                discovery.game_count,
                discovery.build_time_ms,
                if discovery.from_cache {
                    " (cached)"
                } else {
                    ""
                }
            );

            // Merge into config and save
            match config.merge_steam_games(&discovery) {
                Ok(added) if added > 0 => {
                    info!("  Added {} new games to userConfig.json", added);
                }
                Ok(_) => {
                    // No new games to add
                }
                Err(e) => {
                    warn!("  Failed to save discovered games: {}", e);
                }
            }
        } else {
            info!("  Steam not found or no games installed");
        }
    }

    // Create event-driven process watcher (Windows only)
    // This does initial enumeration and sets up WMI event subscription
    #[cfg(windows)]
    let process_watcher = ProcessWatcher::new().await;

    // Create config generation channel for notifying sensors of hot-reload
    let (config_generation_tx, _) = broadcast::channel::<()>(4);

    // Create shared state
    let state = Arc::new(AppState {
        config: RwLock::new(config.clone()),
        mqtt,
        shutdown_tx: shutdown_tx.clone(),
        config_generation: config_generation_tx,
        #[cfg(windows)]
        process_watcher,
        display_off: AtomicBool::new(false),
    });

    // Collect task handles for cleanup
    let mut handles: Vec<TaskHandle> = Vec::new();

    // Start event-driven process watcher if game detection or idle tracking is enabled
    #[cfg(windows)]
    if config.features.game_detection || config.features.idle_tracking {
        let poll_interval = Duration::from_secs(config.intervals.game_sensor.max(5));
        state
            .process_watcher
            .start_background(shutdown_tx.subscribe(), poll_interval);
        info!("  Process watcher started (WMI events with polling fallback)");
    }

    // Command executor always runs (needed for any remote control)
    let command_executor = CommandExecutor::new(Arc::clone(&state), command_rx);
    handles.push(tokio::spawn(command_executor.run()));

    // Conditionally start sensors based on features
    if config.features.game_detection {
        let sensor = GameSensor::new(Arc::clone(&state));
        handles.push(tokio::spawn(sensor.run()));
        info!("  Game detection enabled");
    }

    if config.features.idle_tracking {
        let sensor = IdleSensor::new(Arc::clone(&state));
        handles.push(tokio::spawn(sensor.run()));
        info!("  Idle tracking enabled");
    }

    if config.features.power_events {
        let listener = PowerEventListener::new(Arc::clone(&state));
        handles.push(tokio::spawn(listener.run()));
        info!("  Power events enabled");
    }

    if config.features.system_sensors {
        let sensor = SystemSensor::new(Arc::clone(&state));
        handles.push(tokio::spawn(sensor.run()));
        info!("  System sensors enabled (CPU, memory, battery, active window)");
    }

    #[cfg(windows)]
    if config.features.steam_updates {
        use crate::sensors::SteamSensor;
        let sensor = SteamSensor::new(Arc::clone(&state));
        handles.push(tokio::spawn(sensor.run()));
        info!("  Steam update detection enabled");
    }

    // Custom sensors (if enabled and defined)
    if config.custom_sensors_enabled && !config.custom_sensors.is_empty() {
        let manager = CustomSensorManager::new(Arc::clone(&state));
        handles.push(tokio::spawn(manager.run()));
        state
            .mqtt
            .register_custom_sensors(&config.custom_sensors)
            .await;
        info!(
            "  Custom sensors enabled ({} defined)",
            config.custom_sensors.len()
        );
    }

    // Custom commands (just register discovery, executor handles them)
    if config.custom_commands_enabled && !config.custom_commands.is_empty() {
        state
            .mqtt
            .register_custom_commands(&config.custom_commands)
            .await;
        info!(
            "  Custom commands enabled ({} defined)",
            config.custom_commands.len()
        );
    }

    // Config file watcher for hot-reload
    handles.push(tokio::spawn(config::watch_config(Arc::clone(&state))));

    // Start tray icon (Windows only, runs on separate thread)
    if show_tray {
        let tray_shutdown = shutdown_tx.clone();
        let tray_config_path = config_path.clone();
        std::thread::Builder::new()
            .name("tray".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                tray::run_tray(tray_shutdown, tray_config_path);
            })
            .expect("failed to spawn tray thread");
    }

    // Publish initial availability
    state.mqtt.publish_availability(true).await;

    // Only publish sleep_state if power_events enabled
    if config.features.power_events {
        state
            .mqtt
            .publish_sensor_retained("sleep_state", "awake")
            .await;
        state.mqtt.publish_sensor_retained("display", "on").await;
    }

    // Wait for shutdown signal (Ctrl+C)
    info!("PC Bridge running. Press Ctrl+C to stop.");

    #[cfg(windows)]
    {
        // On Windows, poll custom flag (tokio signal doesn't work with attached console)
        loop {
            if SHUTDOWN_FLAG.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[cfg(not(windows))]
    tokio::signal::ctrl_c().await?;

    info!("Shutting down...");
    let _ = shutdown_tx.send(());

    // Wait for tasks to finish (with timeout)
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for handle in handles {
            let _ = handle.await;
        }
    })
    .await;

    // Publish offline status
    state.mqtt.publish_availability(false).await;

    info!("PC Bridge stopped");

    // Print newline to ensure terminal prompt is on its own line
    println!();

    Ok(())
}

/// Log which features are enabled
fn log_enabled_features(config: &Config) {
    let features = &config.features;
    let count = [
        features.game_detection,
        features.idle_tracking,
        features.power_events,
        features.notifications,
        config.custom_sensors_enabled,
        config.custom_commands_enabled,
    ]
    .iter()
    .filter(|&&x| x)
    .count();

    if count == 0 {
        info!("No features enabled - running in minimal mode");
    } else {
        info!("Features enabled: {}", count);
    }
}

/// Kill any other running instances (platform-specific)
#[cfg(windows)]
fn kill_existing_instances() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

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

        if Process32FirstW(snapshot, &raw mut entry).is_ok() {
            loop {
                let proc_name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_lowercase();

                // Check if this process matches any of our exe names
                let is_match = exe_names.iter().any(|&name| proc_name == name);

                if is_match && entry.th32ProcessID != my_pid {
                    if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                        info!(
                            "Killing existing instance: {} (PID {})",
                            proc_name, entry.th32ProcessID
                        );
                        let _ = TerminateProcess(handle, 0);
                        let _ = CloseHandle(handle);
                    }
                }

                if Process32NextW(snapshot, &raw mut entry).is_err() {
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

    let _my_pid = std::process::id();
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
