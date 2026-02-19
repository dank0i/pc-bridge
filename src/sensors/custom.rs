//! Custom sensor polling - user-defined sensors from config

use log::{debug, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

use crate::AppState;
use crate::config::{CustomSensor, CustomSensorType};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Case-insensitive ASCII substring search without allocation.
/// Uses byte-level sliding window comparison.
#[cfg(windows)]
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Custom sensor manager - polls user-defined sensors
pub struct CustomSensorManager {
    state: Arc<AppState>,
}

impl CustomSensorManager {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // Snapshot config at startup
        let (mut sensors, mut enabled) = {
            let config = self.state.config.read().await;
            (config.custom_sensors.clone(), config.custom_sensors_enabled)
        };

        if !enabled {
            info!("Custom sensors disabled (custom_sensors_enabled=false)");
            return;
        }
        if sensors.is_empty() {
            info!("No custom sensors configured");
            return;
        }

        warn!(
            "Custom sensors ENABLED - {} sensor(s) configured",
            sensors.len()
        );
        for sensor in &sensors {
            info!(
                "  - {} ({:?}, {}s interval)",
                sensor.name, sensor.sensor_type, sensor.interval_seconds
            );
        }

        // Track last poll time per sensor
        let mut last_poll: HashMap<String, tokio::time::Instant> = HashMap::new();

        // Use minimum sensor interval for tick (avoids waking every 1s)
        let min_interval = sensors
            .iter()
            .map(|s| s.interval_seconds)
            .min()
            .unwrap_or(30)
            .max(1);
        let mut tick = interval(Duration::from_secs(min_interval));

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Custom sensor manager shutting down");
                    break;
                }
                _ = config_rx.recv() => {
                    // Hot-reload: re-snapshot config
                    let config = self.state.config.read().await;
                    sensors.clone_from(&config.custom_sensors);
                    enabled = config.custom_sensors_enabled;
                    info!("Custom sensors config reloaded ({} sensors, enabled={})", sensors.len(), enabled);
                }
                _ = tick.tick() => {
                    if !enabled {
                        continue;
                    }

                    let now = tokio::time::Instant::now();

                    for sensor in &sensors {
                        let should_poll = match last_poll.get(&sensor.name) {
                            Some(last) => now.duration_since(*last) >= Duration::from_secs(sensor.interval_seconds),
                            None => true,
                        };

                        if should_poll {
                            let value = self.poll_sensor(sensor).await;

                            // Publish to MQTT
                            let topic_name = format!("custom_{}", sensor.name);
                            self.state.mqtt.publish_sensor(&topic_name, &value).await;
                            debug!("Custom sensor '{}' = {}", sensor.name, value);

                            last_poll.insert(sensor.name.clone(), now);
                        }
                    }
                }
            }
        }
    }

    /// Poll a single custom sensor and return its value
    async fn poll_sensor(&self, sensor: &CustomSensor) -> String {
        match sensor.sensor_type {
            CustomSensorType::Powershell => self.poll_powershell(sensor).await,
            CustomSensorType::ProcessExists => self.poll_process_exists(sensor).await,
            CustomSensorType::FileContents => self.poll_file_contents(sensor).await,
            CustomSensorType::Registry => self.poll_registry(sensor).await,
        }
    }

    /// Execute PowerShell script and return output
    #[cfg(windows)]
    async fn poll_powershell(&self, sensor: &CustomSensor) -> String {
        let script = match &sensor.script {
            Some(s) => s.clone(),
            None => return "error: no script".to_string(),
        };

        let result = tokio::task::spawn_blocking(move || {
            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            match output {
                Ok(out) => {
                    if out.status.success() {
                        String::from_utf8_lossy(&out.stdout).trim().to_string()
                    } else {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        format!("error: {}", stderr.trim())
                    }
                }
                Err(e) => format!("error: {}", e),
            }
        })
        .await;

        result.unwrap_or_else(|e| format!("error: {}", e))
    }

    #[cfg(unix)]
    async fn poll_powershell(&self, _sensor: &CustomSensor) -> String {
        "error: powershell not available on this platform".to_string()
    }

    /// Check if a process exists (uses always-up-to-date process watcher)
    #[cfg(windows)]
    async fn poll_process_exists(&self, sensor: &CustomSensor) -> String {
        let process = match &sensor.process {
            Some(p) => p.to_lowercase(),
            None => return "error: no process".to_string(),
        };

        let state = self.state.process_watcher.state();
        let guard = state.read().await;
        let exists = guard.names().iter().any(|name| {
            name.eq_ignore_ascii_case(&process) || contains_ignore_ascii_case(name, &process)
        });

        if exists { "on" } else { "off" }.to_string()
    }

    #[cfg(unix)]
    async fn poll_process_exists(&self, sensor: &CustomSensor) -> String {
        let process = match &sensor.process {
            Some(p) => p.clone(),
            None => return "error: no process".to_string(),
        };

        let result = tokio::task::spawn_blocking(move || {
            use std::process::Command;
            let output = Command::new("pgrep").args(["-x", &process]).output();

            match output {
                Ok(out) => {
                    if out.status.success() {
                        "on"
                    } else {
                        "off"
                    }
                }
                Err(_) => "error",
            }
        })
        .await;

        result.unwrap_or("error").to_string()
    }

    /// Read file contents
    async fn poll_file_contents(&self, sensor: &CustomSensor) -> String {
        let path = match &sensor.file_path {
            Some(p) => p.clone(),
            None => return "error: no file_path".to_string(),
        };

        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents.trim().to_string(),
            Err(e) => format!("error: {}", e),
        }
    }

    /// Read registry value (Windows only)
    #[cfg(windows)]
    async fn poll_registry(&self, sensor: &CustomSensor) -> String {
        let key = match &sensor.registry_key {
            Some(k) => k.clone(),
            None => return "error: no registry_key".to_string(),
        };
        let value = match &sensor.registry_value {
            Some(v) => v.clone(),
            None => return "error: no registry_value".to_string(),
        };

        let result = tokio::task::spawn_blocking(move || {
            use winreg::RegKey;
            use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

            // Parse key path (e.g., "HKEY_LOCAL_MACHINE\\SOFTWARE\\...")
            let (hive, subkey) = if let Some(rest) = key.strip_prefix("HKEY_LOCAL_MACHINE\\") {
                (HKEY_LOCAL_MACHINE, rest)
            } else if let Some(rest) = key.strip_prefix("HKLM\\") {
                (HKEY_LOCAL_MACHINE, rest)
            } else if let Some(rest) = key.strip_prefix("HKEY_CURRENT_USER\\") {
                (HKEY_CURRENT_USER, rest)
            } else if let Some(rest) = key.strip_prefix("HKCU\\") {
                (HKEY_CURRENT_USER, rest)
            } else {
                return format!("error: unsupported registry hive in '{}'", key);
            };

            let reg_key = match RegKey::predef(hive).open_subkey(subkey) {
                Ok(k) => k,
                Err(e) => return format!("error: {}", e),
            };

            match reg_key.get_value::<String, _>(&value) {
                Ok(v) => v,
                Err(_) => {
                    // Try as DWORD
                    match reg_key.get_value::<u32, _>(&value) {
                        Ok(v) => v.to_string(),
                        Err(e) => format!("error: {}", e),
                    }
                }
            }
        })
        .await;

        result.unwrap_or_else(|e| format!("error: {}", e))
    }

    #[cfg(unix)]
    async fn poll_registry(&self, _sensor: &CustomSensor) -> String {
        "error: registry not available on this platform".to_string()
    }
}
