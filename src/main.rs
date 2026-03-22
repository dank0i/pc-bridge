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
mod credential;
mod mqtt;
mod notification;
mod power;
mod sensors;
mod setup;
mod steam;
mod updater;

use log::{error, info};
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};

/// Saved console mode for restoration on exit (Windows only).
/// Stores the raw handle as `isize` (avoiding `*mut c_void` Send/Sync issues)
/// and the original `CONSOLE_MODE` flags.
#[cfg(windows)]
static ORIGINAL_CONSOLE_MODE: std::sync::OnceLock<(
    isize,
    windows::Win32::System::Console::CONSOLE_MODE,
)> = std::sync::OnceLock::new();

use crate::commands::CommandExecutor;
use crate::config::Config;
use crate::mqtt::MqttClient;
use crate::power::PowerEventListener;
#[cfg(windows)]
use crate::sensors::ProcessWatcher;
use crate::sensors::{CustomSensorManager, GameSensor, IdleSensor, SystemSensor};

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
    /// Monotonic start time for uptime tracking in health diagnostics
    pub start_time: std::time::Instant,
}

/// Handle for optional tasks
type TaskHandle = tokio::task::JoinHandle<()>;

#[cfg(windows)]
fn restore_console_mode() {
    if let Some(&(raw_handle, mode)) = ORIGINAL_CONSOLE_MODE.get() {
        unsafe {
            let handle = windows::Win32::Foundation::HANDLE(raw_handle as *mut core::ffi::c_void);
            let _ = windows::Win32::System::Console::SetConsoleMode(handle, mode);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // On Windows, attach to parent console if launched from terminal
    // This allows seeing output when run from cmd/powershell
    #[cfg(windows)]
    let console_attached;
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{
            AttachConsole, ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
            SetConsoleMode,
        };
        unsafe {
            // ATTACH_PARENT_PROCESS = -1 (0xFFFFFFFF)
            console_attached = AttachConsole(u32::MAX).is_ok();
            if console_attached {
                // Save original console mode so we can restore it on exit.
                // Only add ENABLE_PROCESSED_INPUT (Ctrl+C as signal) without
                // stripping ENABLE_LINE_INPUT / ENABLE_ECHO_INPUT from the
                // parent shell's console.
                if let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) {
                    let mut original_mode =
                        windows::Win32::System::Console::CONSOLE_MODE::default();
                    if GetConsoleMode(handle, &mut original_mode).is_ok() {
                        let _ = ORIGINAL_CONSOLE_MODE.set((handle.0 as isize, original_mode));
                        let _ = SetConsoleMode(handle, original_mode | ENABLE_PROCESSED_INPUT);
                    }
                }
            }
        }
    }

    // Initialize logging
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .format_target(false)
        .format_timestamp_secs()
        .init();

    info!("PC Bridge starting...");

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();
    let force_setup = args.iter().any(|a| a == "--setup");
    let reset_password = args.iter().any(|a| a == "--reset-password");

    // Kill any existing instances
    kill_existing_instances();

    // Clean up leftover .old files from a previous update
    updater::cleanup_old_files();

    // --reset-password: prompt for new MQTT password, encrypt, save, and exit
    if reset_password {
        return reset_mqtt_password();
    }

    // Check for first run or --setup flag
    let first_run = Config::is_first_run()?;
    if force_setup || first_run {
        let existing_config = !first_run;

        if first_run {
            info!("First run detected - launching setup wizard");
        }

        if let Some(setup_config) = setup::run_setup_wizard(existing_config) {
            setup::save_setup_config(&setup_config)?;
            info!("Setup complete! Configuration saved.");
        } else {
            error!("Setup cancelled by user");
            #[cfg(windows)]
            {
                use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};
                use windows::core::w;
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

    // Load configuration (prompt interactively if credential can't be decrypted)
    let config = match Config::load() {
        Ok(c) => c,
        Err(e)
            if e.downcast_ref::<credential::CredentialDecryptFailed>()
                .is_some() =>
        {
            handle_credential_failure()?
        }
        Err(e) => return Err(e),
    };
    info!("Loaded config for device: {}", config.device_name);

    // Log enabled features
    log_enabled_features(&config);

    // Create shutdown channel
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Create MQTT client (conditionally registers discovery based on features)
    let (mqtt, command_rx) = MqttClient::new(&config, shutdown_tx.subscribe()).await?;

    // Steam discovery is deferred to the "refresh_steam_games" button in HA
    // (previously ran at startup, causing ~400KB+ heap fragmentation on Windows)

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
        start_time: std::time::Instant::now(),
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
        info!("  System sensors enabled (CPU/memory polled, battery/active_window event-driven)");
    }

    if config.features.steam_updates {
        use crate::sensors::SteamSensor;
        let sensor = SteamSensor::new(Arc::clone(&state));
        handles.push(tokio::spawn(sensor.run()));
        info!("  Steam update detection enabled (filesystem watcher)");
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

    // Wait for shutdown signal (Ctrl+C or broadcast)
    info!("PC Bridge running. Press Ctrl+C to stop.");

    #[cfg(windows)]
    {
        if console_attached {
            // Terminal mode: wait for Ctrl+C via tokio's signal handler
            tokio::signal::ctrl_c().await.ok();
        } else {
            // Background mode (no console): wait for broadcast shutdown
            let mut shutdown_rx = shutdown_tx.subscribe();
            let _ = shutdown_rx.recv().await;
        }
    }

    #[cfg(not(windows))]
    tokio::signal::ctrl_c().await?;

    info!("Shutting down...");

    // Second Ctrl+C force-exits (in case shutdown hangs)
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("Forced shutdown");
        #[cfg(windows)]
        restore_console_mode();
        std::process::exit(1);
    });

    let _ = shutdown_tx.send(());

    // Wait for tasks to finish (with timeout)
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for handle in handles {
            let _ = handle.await;
        }
    })
    .await;

    // Publish offline status (with timeout to avoid hanging on broken MQTT)
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.mqtt.publish_availability(false),
    )
    .await;

    info!("PC Bridge stopped");

    // Restore parent terminal's console mode before exiting
    #[cfg(windows)]
    restore_console_mode();

    // Print newline to ensure terminal prompt is on its own line
    println!();

    Ok(())
}

/// Log which features are enabled
fn log_enabled_features(config: &Config) {
    let f = &config.features;
    let count = [
        f.game_detection,
        f.idle_tracking,
        f.power_events,
        f.notifications,
        f.system_sensors,
        f.audio_control,
        f.steam_updates,
        f.discord,
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

/// Reset just the MQTT password: load existing config, prompt for new password,
/// encrypt, save, and exit.  Keeps all other settings intact.
fn reset_mqtt_password() -> anyhow::Result<()> {
    use std::io::{self, Write};

    // Allocate console on Windows (GUI subsystem)
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{AllocConsole, SetConsoleTitleW};
        use windows::core::w;
        let _ = AllocConsole();
        let _ = SetConsoleTitleW(w!("PC Bridge - Reset Password"));
    }

    // Load config, ignoring credential errors (we're replacing the password anyway)
    let config = Config::load().or_else(|e| {
        if e.downcast_ref::<credential::CredentialDecryptFailed>()
            .is_some()
        {
            Config::load_without_credential()
        } else {
            Err(e)
        }
    });

    let config = config.unwrap_or_else(|e| {
        eprintln!("Failed to load config: {e}");
        eprintln!("Run with --setup to create a new configuration.");
        std::process::exit(1);
    });

    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║        Reset MQTT Password                ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  Current MQTT broker: {}", config.mqtt.broker);
    println!("  Current MQTT user:   {}", config.mqtt.user);
    println!();
    println!("  Type your new password and press Enter.");
    println!("  Leave blank to cancel.");
    println!();
    print!("  New MQTT password (input is hidden): ");
    io::stdout().flush().ok();

    let new_pass = rpassword::prompt_password("")
        .unwrap_or_default()
        .trim()
        .to_string();

    if new_pass.is_empty() {
        println!();
        println!("  No changes made.");
    } else {
        credential::save_to_file(&new_pass)?;
        println!();
        println!("  Password updated and encrypted.");
        println!("  Restart PC Bridge for the change to take effect.");
    }

    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::FreeConsole;
        let _ = FreeConsole();
    }

    // Print newline to ensure terminal prompt is on its own line
    println!();

    Ok(())
}

/// Pop up a console and prompt for the MQTT password when decryption fails.
fn handle_credential_failure() -> anyhow::Result<Config> {
    use std::io::{self, Write};

    // Allocate console on Windows (GUI subsystem)
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{AllocConsole, SetConsoleTitleW};
        use windows::core::w;
        let _ = AllocConsole();
        let _ = SetConsoleTitleW(w!("PC Bridge - Credential Error"));
    }

    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║   MQTT Password Could Not Be Decrypted   ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  The stored credential could not be decrypted.");
    println!("  This usually means the config was copied");
    println!("  from another machine or Windows user.");
    println!();
    println!("  Type your MQTT password to re-encrypt it");
    println!("  for this machine, or press Enter to skip.");
    println!();
    print!("  MQTT Password (input is hidden): ");
    io::stdout().flush().ok();

    let new_pass = rpassword::prompt_password("")
        .unwrap_or_default()
        .trim()
        .to_string();

    if new_pass.is_empty() {
        println!();
        println!("  Skipped. MQTT authentication will fail.");
        println!("  You can set it later with --reset-password.");
        println!();
        println!("  Press Enter to continue...");
        let _ = io::stdin().read_line(&mut String::new());
    } else {
        credential::save_to_file(&new_pass)?;
        println!();
        println!("  Password saved and encrypted for this machine.");
        println!();
    }

    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::FreeConsole;
        let _ = FreeConsole();
    }

    // Reload config with the new (or empty) credential
    if new_pass.is_empty() {
        let mut config = Config::load_without_credential()?;
        config.mqtt.pass = String::new();
        Ok(config)
    } else {
        Config::load()
    }
}

/// Kill any other running instances (platform-specific)
#[cfg(windows)]
fn kill_existing_instances() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

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

    let my_pid = std::process::id();
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();

    if exe_name.is_empty() {
        return;
    }

    // Use pgrep to find other instances, then kill them excluding our PID
    if let Ok(output) = Command::new("pgrep").args(["-x", &exe_name]).output() {
        let pids = String::from_utf8_lossy(&output.stdout);
        for line in pids.lines() {
            if let Ok(pid) = line.trim().parse::<u32>()
                && pid != my_pid
            {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
    }

    std::thread::sleep(std::time::Duration::from_millis(1000));
}
