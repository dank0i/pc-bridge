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
mod fsutil;
mod hwinfo;
#[cfg(unix)]
mod linux_dbus;
#[cfg(unix)]
mod linux_idle;
#[cfg(unix)]
mod linux_wayland;
#[cfg(unix)]
mod linux_x11;
mod logging;
mod mqtt;
mod notification;
mod power;
mod sensors;
mod setup;
mod steam;
mod supervisor;
mod ui;
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
#[cfg(windows)]
use crate::sensors::ProcessWatcher;

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
    /// When true, commands are resolved and reported to the test topic but
    /// their OS side effects are NOT performed. Enabled via `--dry-run` or
    /// `PC_BRIDGE_DRY_RUN=1` for the integration test kit; off in normal use.
    pub dry_run: bool,
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

fn main() -> anyhow::Result<()> {
    // The settings window runs in its own mode; the headless agent never loads egui.
    if std::env::args().any(|a| a == "--ui") {
        return ui::run();
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_agent())
}

async fn run_agent() -> anyhow::Result<()> {
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
                    if GetConsoleMode(handle, &raw mut original_mode).is_ok() {
                        let _ = ORIGINAL_CONSOLE_MODE.set((handle.0 as isize, original_mode));
                        let _ = SetConsoleMode(handle, original_mode | ENABLE_PROCESSED_INPUT);
                    }
                }
            }
        }
    }

    // Initialize logging (rotating file sink + stderr mirror)
    logging::init();

    info!("PC Bridge starting...");

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();
    let force_setup = args.iter().any(|a| a == "--setup");
    let reset_password = args.iter().any(|a| a == "--reset-password");
    let dry_run = args.iter().any(|a| a == "--dry-run")
        || matches!(
            std::env::var("PC_BRIDGE_DRY_RUN").as_deref(),
            Ok("1" | "true")
        );

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
        if first_run {
            info!("First run detected - opening the settings window for setup");
        }

        // Prefer the GUI settings window (the same one used for ongoing edits):
        // setup is a native form, not a console wizard. On a headless host where
        // no window can be created, run_native fails, so fall back to the
        // terminal wizard.
        let gui_shown = ui::run().is_ok();
        if !gui_shown {
            info!("No display available - falling back to the terminal setup wizard");
            if let Some(setup_config) = setup::run_setup_wizard(!first_run) {
                setup::save_setup_config(&setup_config)?;
            }
        }

        // The user must have saved a valid config in the window/wizard to
        // continue; if none exists, setup was closed without finishing.
        if Config::is_first_run()? {
            error!("Setup was not completed - exiting");
            #[cfg(windows)]
            {
                use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};
                use windows::core::w;
                unsafe {
                    MessageBoxW(
                        None,
                        w!("Setup was not completed.\n\nPC Bridge will now exit."),
                        w!("PC Bridge"),
                        MB_OK | MB_ICONWARNING,
                    );
                }
            }
            return Ok(());
        }
        info!("Setup complete! Configuration saved.");
    }

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

    // Check for updates (non-blocking, continues after check)
    tokio::spawn(updater::check_for_updates(config.update_channel.clone()));

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
        dry_run,
    });
    if dry_run {
        info!(
            "DRY-RUN mode enabled: commands report to the test topic, OS side effects are skipped"
        );
    }

    // Collect task handles for cleanup
    let mut handles: Vec<TaskHandle> = Vec::new();

    // Start event-driven process watcher if game detection or idle tracking is enabled
    #[cfg(windows)]
    if config.features.running_game || config.features.idle_tracking {
        let poll_interval = Duration::from_secs(config.intervals.game_sensor.max(5));
        state
            .process_watcher
            .start_background(shutdown_tx.subscribe(), poll_interval);
        info!("  Process watcher started (WMI events with polling fallback)");
    }

    // Command executor always runs (needed for any remote control)
    let command_executor = CommandExecutor::new(Arc::clone(&state), command_rx);
    handles.push(tokio::spawn(command_executor.run()));

    // Re-publish HA discovery on every MQTT reconnect. A broker that restarts
    // without persistence loses the retained config topics, which would orphan
    // all entities until the agent restarts; re-registering restores them.
    {
        let state = Arc::clone(&state);
        let mut reconnect_rx = state.mqtt.subscribe_reconnect();
        let mut shutdown_rx = state.shutdown_tx.subscribe();
        handles.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    // Observe shutdown so this task exits promptly instead of
                    // blocking the join window (the reconnect sender lives in
                    // AppState and never drops, so recv() alone never returns).
                    _ = shutdown_rx.recv() => break,
                    r = reconnect_rx.recv() => match r {
                        // A reconnect (Ok), or we fell behind the reconnect signals
                        // (Lagged) - either way re-register (idempotent). Lagged
                        // must NOT end the loop, or a burst of broker flaps would
                        // silently stop discovery re-registration and orphan HA
                        // entities.
                        Ok(())
                        | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let config = state.config.read().await;
                            // Re-register only. We do NOT re-run clear_disabled_entities
                            // here: re-registration alone restores any config a broker
                            // restart dropped, and the disabled entities were already
                            // cleared at startup (and on hot-reload when toggled off), so
                            // repeating the ~3x-per-entity teardown on every reconnect is
                            // pure churn on a flapping broker.
                            state.mqtt.register_discovery(&config).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        }));
    }

    // All sensors except HWiNFO are now started/stopped live by the supervisor
    // (see its spawn below) as their feature flags change - including the
    // thread-holding ones (system, session, now_playing, power), which take a
    // per-task shutdown into run(). Only HWiNFO (Windows-only) stays startup-gated.

    #[cfg(windows)]
    if config.features.hwinfo_sensor {
        use crate::sensors::hwinfo::HwInfoSensor;
        let sensor = HwInfoSensor::new(Arc::clone(&state));
        handles.push(tokio::spawn(sensor.run()));
        info!("  HWiNFO sensor enabled (Global\\HWiNFO_SENS_SM2 lazy poll @ 500ms)");
    }

    // (all no-persistent-thread sensors are supervised; see the supervisor spawn below)

    // Custom sensors: the manager TASK is supervised (started below); the
    // discovery registration is done here once (and on hot-reload).
    if config.custom_sensors_enabled && !config.custom_sensors.is_empty() {
        state
            .mqtt
            .register_custom_sensors(&config.custom_sensors)
            .await;
        info!(
            "  Custom sensors enabled ({} defined)",
            config.custom_sensors.len()
        );
    }

    // Runtime supervisor: starts/stops every sensor task live as feature flags
    // change (no restart). Only HWiNFO stays startup-gated above.
    handles.push(tokio::spawn(
        supervisor::Supervisor::new(Arc::clone(&state)).run(),
    ));

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

    // HWiNFO sensors use a multi-source availability list - pre-seed the
    // HWiNFO availability topic to "offline" so HA marks the entities
    // unavailable until the sensor task detects HWiNFO is running. Gated
    // to Windows because the producer task is Windows-only; we don't want
    // a stray flag on Linux/macOS publishing a retained topic HA would
    // then carry around forever.
    #[cfg(windows)]
    if config.features.hwinfo_sensor {
        state.mqtt.publish_hwinfo_availability(false).await;
    }

    // Seed initial sensor states, each gated by its own flag.
    if config.features.sleep_wake {
        state
            .mqtt
            .publish_sensor_retained("sleep_state", "awake")
            .await;
    }
    if config.features.display_state {
        state.mqtt.publish_sensor_retained("display", "on").await;
    }
    if config.features.session_state {
        state
            .mqtt
            .publish_sensor_retained("session", "unlocked")
            .await;
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
        f.running_game,
        f.game_catalog,
        f.steam_library,
        f.launch_game,
        f.close_game,
        f.idle_tracking,
        f.sleep_wake,
        f.display_state,
        f.cmd_shutdown,
        f.cmd_restart,
        f.cmd_sleep,
        f.cmd_lock,
        f.cmd_logoff,
        f.cmd_monitor,
        f.notifications,
        f.cpu_sensor,
        f.memory_sensor,
        f.active_window,
        f.session_state,
        f.audio_device,
        f.mic,
        f.webcam,
        f.now_playing,
        f.volume,
        f.media_controls,
        f.steam_updates,
        f.discord,
        f.gpu_sensor,
        f.network_sensor,
        f.disk_sensor,
        f.uptime_sensor,
        f.hwinfo_sensor,
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

                if is_match
                    && entry.th32ProcessID != my_pid
                    && let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID)
                {
                    info!(
                        "Killing existing instance: {} (PID {})",
                        proc_name, entry.th32ProcessID
                    );
                    let _ = TerminateProcess(handle, 0);
                    let _ = CloseHandle(handle);
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

    std::thread::sleep(std::time::Duration::from_secs(1));
}
