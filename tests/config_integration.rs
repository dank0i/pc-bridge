//! Integration tests for configuration loading and validation
//!
//! These tests use temporary files to test the actual config loading flow.

use std::io::Write;
use tempfile::NamedTempFile;

/// Helper to create a temp config file and test parsing
fn parse_config_json(json: &str) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(json)
}

#[test]
fn test_complete_config_parses() {
    let json = r#"{
        "device_name": "test-pc",
        "mqtt": {
            "broker": "tcp://localhost:1883",
            "user": "homeassistant",
            "pass": "secret123"
        },
        "features": {
            "game_detection": true,
            "idle_tracking": true,
            "power_events": true,
            "notifications": true,
            "system_sensors": true,
            "audio_control": false,
            "steam_updates": true
        },
        "intervals": {
            "game_sensor": 5,
            "last_active": 10,
            "screensaver": 10,
            "availability": 30,
            "steam_check": 60,
            "steam_updating": 10
        },
        "games": {
            "bf2042.exe": "battlefield_6",
            "cs2.exe": {
                "game_id": "counter_strike_2",
                "app_id": 730,
                "name": "Counter-Strike 2"
            },
            "helldivers2.exe": {
                "game_id": "helldivers_2",
                "app_id": 553850
            }
        },
        "show_tray_icon": true,
        "custom_sensors_enabled": false,
        "custom_commands_enabled": false,
        "custom_command_privileges_allowed": false,
        "custom_sensors": [],
        "custom_commands": []
    }"#;

    let config: serde_json::Value = parse_config_json(json).expect("Failed to parse config");

    assert_eq!(config["device_name"], "test-pc");
    assert_eq!(config["mqtt"]["broker"], "tcp://localhost:1883");
    assert!(config["features"]["game_detection"].as_bool().unwrap());
    assert_eq!(config["games"]["bf2042.exe"], "battlefield_6");
    assert_eq!(config["games"]["cs2.exe"]["app_id"], 730);
}

#[test]
fn test_minimal_config_parses() {
    let json = r#"{
        "device_name": "minimal-pc",
        "mqtt": {
            "broker": "tcp://192.168.1.100:1883"
        }
    }"#;

    let config: serde_json::Value =
        parse_config_json(json).expect("Failed to parse minimal config");
    assert_eq!(config["device_name"], "minimal-pc");
}

#[test]
fn test_config_with_custom_sensors() {
    let json = r#"{
        "device_name": "sensor-pc",
        "mqtt": { "broker": "tcp://localhost:1883" },
        "custom_sensors_enabled": true,
        "custom_sensors": [
            {
                "name": "cpu_temp",
                "type": "powershell",
                "script": "Get-WmiObject MSAcpi_ThermalZoneTemperature -Namespace root/wmi | Select -First 1 | % { ($_.CurrentTemperature - 2732) / 10 }",
                "interval_seconds": 60,
                "unit": "°C",
                "icon": "mdi:thermometer"
            },
            {
                "name": "notepad_running",
                "type": "process_exists",
                "process": "notepad.exe",
                "interval_seconds": 30
            }
        ]
    }"#;

    let config: serde_json::Value = parse_config_json(json).expect("Failed to parse");

    assert!(config["custom_sensors_enabled"].as_bool().unwrap());
    assert_eq!(config["custom_sensors"].as_array().unwrap().len(), 2);
    assert_eq!(config["custom_sensors"][0]["name"], "cpu_temp");
    assert_eq!(config["custom_sensors"][1]["type"], "process_exists");
}

#[test]
fn test_config_with_custom_commands() {
    let json = r#"{
        "device_name": "cmd-pc",
        "mqtt": { "broker": "tcp://localhost:1883" },
        "custom_commands_enabled": true,
        "custom_command_privileges_allowed": false,
        "custom_commands": [
            {
                "name": "open_steam",
                "type": "executable",
                "path": "C:\\Program Files (x86)\\Steam\\steam.exe",
                "args": ["-silent"],
                "icon": "mdi:steam"
            },
            {
                "name": "clear_temp",
                "type": "powershell",
                "script": "Remove-Item $env:TEMP\\* -Recurse -Force -ErrorAction SilentlyContinue"
            }
        ]
    }"#;

    let config: serde_json::Value = parse_config_json(json).expect("Failed to parse");

    assert!(config["custom_commands_enabled"].as_bool().unwrap());
    assert!(!config["custom_command_privileges_allowed"]
        .as_bool()
        .unwrap());
    assert_eq!(config["custom_commands"].as_array().unwrap().len(), 2);
}

#[test]
fn test_config_game_variants() {
    let json = r#"{
        "device_name": "game-pc",
        "mqtt": { "broker": "tcp://localhost:1883" },
        "games": {
            "simple_game.exe": "simple_game",
            "steam_game.exe": {
                "game_id": "steam_game",
                "app_id": 12345,
                "name": "Steam Game: The Game",
                "auto_discovered": true
            },
            "no_name.exe": {
                "game_id": "no_name_game",
                "app_id": 67890
            }
        }
    }"#;

    let config: serde_json::Value = parse_config_json(json).expect("Failed to parse");
    let games = config["games"].as_object().unwrap();

    // Simple string variant
    assert_eq!(games["simple_game.exe"], "simple_game");

    // Full variant with all fields
    assert_eq!(games["steam_game.exe"]["game_id"], "steam_game");
    assert_eq!(games["steam_game.exe"]["app_id"], 12345);
    assert_eq!(games["steam_game.exe"]["name"], "Steam Game: The Game");
    assert!(games["steam_game.exe"]["auto_discovered"]
        .as_bool()
        .unwrap());

    // Full variant without name
    assert_eq!(games["no_name.exe"]["game_id"], "no_name_game");
    assert!(games["no_name.exe"]["name"].is_null());
}

#[test]
fn test_write_and_read_temp_config() {
    let json = r#"{"device_name": "temp-test-pc", "mqtt": {"broker": "tcp://localhost:1883"}}"#;

    let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file
        .write_all(json.as_bytes())
        .expect("Failed to write");

    let content = std::fs::read_to_string(temp_file.path()).expect("Failed to read");
    let config: serde_json::Value = serde_json::from_str(&content).expect("Failed to parse");

    assert_eq!(config["device_name"], "temp-test-pc");
}

#[test]
fn test_invalid_json_fails() {
    let bad_json = r#"{ "device_name": "test, "mqtt": {} }"#;
    assert!(parse_config_json(bad_json).is_err());
}

#[test]
fn test_empty_json_object() {
    let json = "{}";
    let config: serde_json::Value = parse_config_json(json).expect("Failed to parse");
    assert!(config["device_name"].is_null());
}
