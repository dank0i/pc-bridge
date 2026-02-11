//! First-run setup wizard using console input
//!
//! Guides users through initial configuration with a clean text UI.

use std::io::{self, Write};
use std::path::PathBuf;
use tracing::info;

/// Configuration collected from setup wizard
#[derive(Debug)]
pub struct SetupConfig {
    pub device_name: String,
    pub mqtt_broker: String,
    pub mqtt_user: String,
    pub mqtt_pass: String,
    pub game_detection: bool,
    pub idle_tracking: bool,
    pub power_events: bool,
    pub notifications: bool,
    pub system_sensors: bool,
    pub audio_control: bool,
    pub steam_updates: bool,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            device_name: get_default_device_name(),
            mqtt_broker: "tcp://homeassistant.local:1883".to_string(),
            mqtt_user: String::new(),
            mqtt_pass: String::new(),
            game_detection: true,
            idle_tracking: true,
            power_events: true,
            notifications: true,
            system_sensors: true,
            audio_control: true,
            steam_updates: true,
        }
    }
}

/// Get default device name from hostname
fn get_default_device_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "my-pc".to_string())
        .to_lowercase()
        .replace(' ', "-")
}

/// Clear screen (Windows)
#[cfg(windows)]
fn clear_screen() {
    use windows::Win32::System::Console::*;
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
        let mut info = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(handle, &mut info).is_ok() {
            let size = info.dwSize.X as u32 * info.dwSize.Y as u32;
            let mut written = 0;
            let _ = FillConsoleOutputCharacterA(
                handle,
                b' ' as i8,
                size,
                Default::default(),
                &mut written,
            );
            let _ = SetConsoleCursorPosition(handle, Default::default());
        }
    }
}

#[cfg(not(windows))]
fn clear_screen() {
    print!("\x1B[2J\x1B[1;1H");
}

/// Print a header box
fn print_header(title: &str) {
    let width = 44;
    let padding = (width - 2 - title.len()) / 2;
    println!("╔{}╗", "═".repeat(width - 2));
    println!(
        "║{}{}{} ║",
        " ".repeat(padding),
        title,
        " ".repeat(width - 3 - padding - title.len())
    );
    println!("╚{}╝", "═".repeat(width - 2));
    println!();
}

/// Read a line of input with a prompt
fn read_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    input.trim().to_string()
}

/// Run the setup wizard (Windows)
#[cfg(windows)]
pub fn run_setup_wizard() -> Option<SetupConfig> {
    use windows::core::w;
    use windows::Win32::System::Console::*;

    // Allocate console for input (we're a GUI app)
    unsafe {
        let _ = AllocConsole();
        // Set console title
        let _ = SetConsoleTitleW(w!("PC Bridge Setup"));
    }

    let result = run_wizard_flow();

    unsafe {
        let _ = FreeConsole();
    }

    result
}

/// The actual wizard flow (shared logic)
fn run_wizard_flow() -> Option<SetupConfig> {
    let mut config = SetupConfig::default();

    // Welcome
    clear_screen();
    print_header("PC Bridge Setup");
    println!("  Welcome! This wizard will configure your");
    println!("  connection to Home Assistant via MQTT.");
    println!();
    println!("  Press Enter to continue...");
    read_input("");

    // Device name
    loop {
        clear_screen();
        print_header("Device Name");
        println!("  Enter a unique name for this PC.");
        println!("  Used as the MQTT client ID and entity prefix.");
        println!();
        println!("  • No spaces allowed");
        println!("  • Lowercase recommended");
        println!();
        let input = read_input(&format!("  Name [{}]: ", config.device_name));

        if input.is_empty() {
            break; // Use default
        } else if input.contains(' ') {
            println!("\n  Name cannot contain spaces!");
            read_input("  Press Enter to try again...");
        } else {
            config.device_name = input;
            break;
        }
    }

    // MQTT Broker
    clear_screen();
    print_header("MQTT Broker");
    println!("  Enter your MQTT broker address.");
    println!();
    println!("  Examples:");
    println!("    tcp://homeassistant.local:1883");
    println!("    tcp://192.168.1.100:1883");
    println!();
    let input = read_input(&format!("  Broker [{}]: ", config.mqtt_broker));
    if !input.is_empty() {
        config.mqtt_broker = input;
    }

    // MQTT Username
    clear_screen();
    print_header("MQTT Authentication");
    println!("  Enter MQTT credentials if required.");
    println!("  Leave blank for anonymous access.");
    println!();
    let input = read_input("  Username: ");
    config.mqtt_user = input;

    // MQTT Password (only if username provided)
    if !config.mqtt_user.is_empty() {
        println!();
        let input = read_input("  Password: ");
        config.mqtt_pass = input;
    }

    // Feature selection with toggle menu
    loop {
        clear_screen();
        print_header("Features");
        println!("  Select which features to enable.");
        println!("  Type a number to toggle, Enter when done.");
        println!();
        println!(
            "  [1] [{}] Game Detection",
            if config.game_detection { "*" } else { " " }
        );
        println!("      Detect running games, report to HA");
        println!();
        println!(
            "  [2] [{}] Idle Tracking",
            if config.idle_tracking { "*" } else { " " }
        );
        println!("      Track keyboard/mouse activity");
        println!();
        println!(
            "  [3] [{}] Power Events",
            if config.power_events { "*" } else { " " }
        );
        println!("      Report sleep/wake/shutdown");
        println!();
        println!(
            "  [4] [{}] Notifications",
            if config.notifications { "*" } else { " " }
        );
        println!("      Receive toast notifications from HA");
        println!();
        println!(
            "  [5] [{}] System Sensors",
            if config.system_sensors { "*" } else { " " }
        );
        println!("      CPU, memory, battery, active window");
        println!();
        println!(
            "  [6] [{}] Audio Control",
            if config.audio_control { "*" } else { " " }
        );
        println!("      Volume, mute, media keys");
        println!();
        println!(
            "  [7] [{}] Steam Updates",
            if config.steam_updates { "*" } else { " " }
        );
        println!("      Detect when Steam games are updating");
        println!();

        let input = read_input("  Toggle (1-7) or Enter to continue: ");

        match input.as_str() {
            "1" => config.game_detection = !config.game_detection,
            "2" => config.idle_tracking = !config.idle_tracking,
            "3" => config.power_events = !config.power_events,
            "4" => config.notifications = !config.notifications,
            "5" => config.system_sensors = !config.system_sensors,
            "6" => config.audio_control = !config.audio_control,
            "7" => config.steam_updates = !config.steam_updates,
            "" => break,
            _ => {} // Ignore invalid input
        }
    }

    // Confirmation
    clear_screen();
    print_header("Confirm Configuration");
    println!("  Device:   {}", config.device_name);
    println!("  Broker:   {}", config.mqtt_broker);
    println!(
        "  Auth:     {}",
        if config.mqtt_user.is_empty() {
            "none"
        } else {
            "configured"
        }
    );
    println!();
    println!("  Features:");
    println!(
        "    [{}] Game Detection",
        if config.game_detection { "x" } else { " " }
    );
    println!(
        "    [{}] Idle Tracking",
        if config.idle_tracking { "x" } else { " " }
    );
    println!(
        "    [{}] Power Events",
        if config.power_events { "x" } else { " " }
    );
    println!(
        "    [{}] Notifications",
        if config.notifications { "x" } else { " " }
    );
    println!(
        "    [{}] System Sensors",
        if config.system_sensors { "x" } else { " " }
    );
    println!(
        "    [{}] Audio Control",
        if config.audio_control { "x" } else { " " }
    );
    println!(
        "    [{}] Steam Updates",
        if config.steam_updates { "x" } else { " " }
    );
    println!();

    let input = read_input("  Save configuration? [Y/n]: ");
    let confirmed = input.is_empty() || input.to_lowercase().starts_with('y');

    if confirmed {
        Some(config)
    } else {
        None
    }
}

/// Save the setup configuration to disk
pub fn save_setup_config(config: &SetupConfig) -> std::io::Result<PathBuf> {
    use crate::config::{Config, FeatureConfig, IntervalConfig, MqttConfig};
    use std::collections::HashMap;

    let full_config = Config {
        device_name: config.device_name.clone(),
        mqtt: MqttConfig {
            broker: config.mqtt_broker.clone(),
            user: config.mqtt_user.clone(),
            pass: config.mqtt_pass.clone(),
            client_id: None,
        },
        intervals: IntervalConfig::default(),
        features: FeatureConfig {
            game_detection: config.game_detection,
            idle_tracking: config.idle_tracking,
            power_events: config.power_events,
            notifications: config.notifications,
            system_sensors: config.system_sensors,
            audio_control: config.audio_control,
            steam_updates: config.steam_updates,
            ..FeatureConfig::default()
        },
        games: HashMap::new(),
        show_tray_icon: None,
        custom_sensors_enabled: false,
        custom_commands_enabled: false,
        custom_command_privileges_allowed: false,
        custom_sensors: Vec::new(),
        custom_commands: Vec::new(),
    };

    let config_path = crate::config::Config::config_path().map_err(std::io::Error::other)?;

    // Create directory if needed
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&full_config).map_err(std::io::Error::other)?;

    std::fs::write(&config_path, json)?;

    info!("Configuration saved to {:?}", config_path);
    Ok(config_path)
}

/// Non-Windows stub
#[cfg(not(windows))]
pub fn run_setup_wizard() -> Option<SetupConfig> {
    run_wizard_flow()
}
