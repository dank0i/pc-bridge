//! Custom sensor polling - user-defined sensors from config

use std::sync::Arc;
use std::time::Duration;
use std::collections::HashMap;
use tokio::time::interval;
use tracing::{info, warn, debug};

use crate::AppState;
use crate::config::{CustomSensor, CustomSensorType};

#[cfg(windows)]
use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

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
        
        // Check if custom sensors are enabled
        {
            let config = self.state.config.read().await;
            if !config.custom_sensors_enabled {
                info!("Custom sensors disabled (custom_sensors_enabled=false)");
                return;
            }
            if config.custom_sensors.is_empty() {
                info!("No custom sensors configured");
                return;
            }
            
            warn!("⚠️  Custom sensors ENABLED - {} sensor(s) configured", config.custom_sensors.len());
            for sensor in &config.custom_sensors {
                info!("  - {} ({:?}, {}s interval)", sensor.name, sensor.sensor_type, sensor.interval_seconds);
            }
        }

        // Track last poll time per sensor
        let mut last_poll: HashMap<String, tokio::time::Instant> = HashMap::new();
        
        // Use minimum sensor interval for tick (avoids waking every 1s)
        let min_interval = {
            let config = self.state.config.read().await;
            config.custom_sensors.iter()
                .map(|s| s.interval_seconds)
                .min()
                .unwrap_or(30)
                .max(1) // At least 1 second
        };
        let mut tick = interval(Duration::from_secs(min_interval));

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Custom sensor manager shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let config = self.state.config.read().await;
                    
                    if !config.custom_sensors_enabled {
                        continue;
                    }
                    
                    let now = tokio::time::Instant::now();
                    
                    for sensor in &config.custom_sensors {
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
        }).await;

        result.unwrap_or_else(|e| format!("error: {}", e))
    }

    #[cfg(unix)]
    async fn poll_powershell(&self, _sensor: &CustomSensor) -> String {
        "error: powershell not available on this platform".to_string()
    }

    /// Check if a process exists
    #[cfg(windows)]
    async fn poll_process_exists(&self, sensor: &CustomSensor) -> String {
        let process = match &sensor.process {
            Some(p) => p.to_lowercase(),
            None => return "error: no process".to_string(),
        };

        use windows::Win32::System::Diagnostics::ToolHelp::*;
        use windows::Win32::Foundation::CloseHandle;

        let exists = unsafe {
            let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                Ok(s) => s,
                Err(_) => return "error: snapshot failed".to_string(),
            };

            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };

            let mut found = false;
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let name = String::from_utf16_lossy(&entry.szExeFile)
                        .trim_end_matches('\0')
                        .to_lowercase();

                    if name == process || name.contains(&process) {
                        found = true;
                        break;
                    }

                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }

            let _ = CloseHandle(snapshot);
            found
        };

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
            let output = Command::new("pgrep")
                .args(["-x", &process])
                .output();

            match output {
                Ok(out) => if out.status.success() { "on" } else { "off" },
                Err(_) => "error",
            }
        }).await;

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
            use winreg::enums::*;
            use winreg::RegKey;

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
        }).await;

        result.unwrap_or_else(|e| format!("error: {}", e))
    }

    #[cfg(unix)]
    async fn poll_registry(&self, _sensor: &CustomSensor) -> String {
        "error: registry not available on this platform".to_string()
    }
}
