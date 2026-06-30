//! HA MQTT discovery registration.
//!
//! Builds and publishes the `homeassistant/<component>/<device>/<entity>/config`
//! payloads that tell HA which entities to auto-create.  `register_discovery`
//! orchestrates all the conditional per-feature registrations; the smaller
//! helpers below build single payloads.

use std::sync::Arc;

use log::{debug, error, info};
use rumqttc::QoS;

use super::payload::{HADevice, HADiscoveryPayload, derive_state_class};
// AvailabilityEntry is only constructed in the Windows-only HWiNFO registration.
#[cfg(windows)]
use super::payload::AvailabilityEntry;
use super::{DISCOVERY_PREFIX, MqttClient};
use crate::config::{Config, CustomCommand, CustomSensor};

impl MqttClient {
    pub(super) async fn register_discovery(&self, config: &Config) {
        // Fix #5: Use shared device reference instead of creating new one
        let device = &self.device;

        // Conditionally register sensors based on features
        if config.features.running_game {
            self.register_sensor_with_attributes(
                device,
                "runninggames",
                "Running Game",
                "mdi:gamepad-variant",
                None,
                None,
            )
            .await;
        }
        if config.features.game_catalog {
            self.register_sensor_with_attributes(
                device,
                "game_catalog",
                "Game Catalog",
                "mdi:gamepad-variant-outline",
                None,
                None,
            )
            .await;
        }

        if config.features.idle_tracking {
            self.register_sensor(
                device,
                "lastactive",
                "Last Active",
                "mdi:clock-outline",
                Some("timestamp"),
                None,
            )
            .await;
            // Seconds since last keyboard/mouse input - numeric, HA-automation friendly.
            // Grows while idle, resets to ~0 on activity.
            self.register_sensor(
                device,
                "idle_seconds",
                "Idle Time",
                "mdi:timer-sand",
                Some("duration"),
                Some("s"),
            )
            .await;
            self.register_sensor(
                device,
                "screensaver",
                "Screensaver",
                "mdi:monitor-shimmer",
                None,
                None,
            )
            .await;
        }

        if config.features.sleep_wake {
            // sleep_state has no availability (always published)
            let payload = HADiscoveryPayload {
                name: "Sleep State".to_string(),
                unique_id: format!("{}_sleep_state", self.device_id),
                state_topic: Some(self.sensor_topic("sleep_state")),
                command_topic: None,
                availability_topic: None,
                availability: None,
                availability_mode: None,
                device: Arc::clone(device),
                icon: Some("mdi:power-sleep".to_string()),
                device_class: None,
                unit_of_measurement: None,
                state_class: None,
                json_attributes_topic: None,
            };
            let topic = self.config_topic("sensor", "sleep_state");
            let Ok(json) = serde_json::to_string(&payload) else {
                error!("Failed to serialize HA discovery payload");
                return;
            };
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;
        }

        // Display power state sensor
        if config.features.display_state {
            self.register_sensor(device, "display", "Display", "mdi:monitor", None, None)
                .await;
        }

        // Session lock/unlock sensor (Windows-only producer via WTS).
        #[cfg(windows)]
        if config.features.session_state {
            self.register_sensor(
                device,
                "session",
                "Session State",
                "mdi:account-lock",
                None,
                None,
            )
            .await;
        }

        // Default audio output device sensor (Windows-only producer via WASAPI).
        #[cfg(windows)]
        if config.features.audio_device {
            self.register_sensor(
                device,
                "audio_device",
                "Default Audio Device",
                "mdi:speaker",
                None,
                None,
            )
            .await;
        }

        // Mic / webcam in-use sensors (Windows-only producer via consent store).
        #[cfg(windows)]
        if config.features.mic {
            self.register_sensor(
                device,
                "mic",
                "Microphone In Use",
                "mdi:microphone",
                None,
                None,
            )
            .await;
        }
        #[cfg(windows)]
        if config.features.webcam {
            self.register_sensor(device, "webcam", "Webcam In Use", "mdi:webcam", None, None)
                .await;
        }

        // System sensors, split into independent flags. Battery and bridge
        // health ride along whenever the system task runs (any of the three on).
        let system_any = config.features.cpu_sensor
            || config.features.memory_sensor
            || config.features.active_window;
        if config.features.cpu_sensor {
            self.register_sensor(
                device,
                "cpu_usage",
                "CPU Usage",
                "mdi:cpu-64-bit",
                None,
                Some("%"),
            )
            .await;
        }
        if config.features.memory_sensor {
            self.register_sensor(
                device,
                "memory_usage",
                "Memory Usage",
                "mdi:memory",
                None,
                Some("%"),
            )
            .await;
        }
        if config.features.active_window {
            self.register_sensor(
                device,
                "active_window",
                "Active Window",
                "mdi:application",
                None,
                None,
            )
            .await;
        }
        if system_any {
            self.register_sensor(
                device,
                "battery_level",
                "Battery Level",
                "mdi:battery",
                Some("battery"),
                Some("%"),
            )
            .await;
            self.register_sensor(
                device,
                "battery_charging",
                "Battery Charging",
                "mdi:battery-charging",
                None,
                None,
            )
            .await;

            // Bridge health diagnostics (uptime in seconds, version in attributes)
            self.register_sensor_with_attributes(
                device,
                "bridge_health",
                "Bridge Health",
                "mdi:heart-pulse",
                Some("duration"),
                Some("s"),
            )
            .await;
        }

        // Steam update sensor - no availability so updates persist while PC is off/asleep
        if config.features.steam_updates {
            let payload = HADiscoveryPayload {
                name: "Steam Updating".to_string(),
                unique_id: format!("{}_steam_updating", self.device_id),
                state_topic: Some(self.sensor_topic("steam_updating")),
                command_topic: None,
                availability_topic: None,
                availability: None,
                availability_mode: None,
                json_attributes_topic: Some(self.sensor_attributes_topic("steam_updating")),
                device: Arc::clone(device),
                icon: Some("mdi:steam".to_string()),
                device_class: None,
                unit_of_measurement: None,
                state_class: None,
            };
            let topic = self.config_topic("sensor", "steam_updating");
            let Ok(json) = serde_json::to_string(&payload) else {
                error!("Failed to serialize HA discovery payload");
                return;
            };
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;
        }

        // GPU sensor
        if config.features.gpu_sensor {
            self.register_sensor(
                device,
                "gpu_usage",
                "GPU Usage",
                "mdi:expansion-card",
                None,
                Some("%"),
            )
            .await;
        }

        // Network throughput sensor
        if config.features.network_sensor {
            self.register_sensor_with_attributes(
                device,
                "network_throughput",
                "Network Throughput",
                "mdi:network",
                None,
                None,
            )
            .await;
        }

        // Disk usage sensor
        if config.features.disk_sensor {
            self.register_sensor_with_attributes(
                device,
                "disk_usage",
                "Disk Usage",
                "mdi:harddisk",
                None,
                Some("%"),
            )
            .await;
        }

        // System uptime sensor
        if config.features.uptime_sensor {
            self.register_sensor(
                device,
                "system_uptime",
                "System Uptime",
                "mdi:clock-check",
                Some("duration"),
                Some("s"),
            )
            .await;
        }

        // HWiNFO sensors are Windows-only - the producer task is
        // `#[cfg(windows)]` and shared-memory is a Win32-only API. We also
        // gate discovery here so a stray `hwinfo_sensor: true` on Linux/macOS
        // doesn't pollute HA with 15 perma-unavailable entities via retained
        // discovery messages.
        #[cfg(windows)]
        if config.features.hwinfo_sensor {
            // Temperatures
            self.register_hwinfo_sensor(
                device,
                "cpu_package_temp",
                "CPU Package Temperature",
                "mdi:thermometer",
                Some("temperature"),
                Some("°C"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_temp",
                "GPU Temperature",
                "mdi:thermometer",
                Some("temperature"),
                Some("°C"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_hotspot_temp",
                "GPU Hot Spot Temperature",
                "mdi:thermometer-alert",
                Some("temperature"),
                Some("°C"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_memory_temp",
                "GPU Memory Temperature",
                "mdi:thermometer-lines",
                Some("temperature"),
                Some("°C"),
            )
            .await;

            // Power
            self.register_hwinfo_sensor(
                device,
                "cpu_package_power",
                "CPU Package Power",
                "mdi:flash",
                Some("power"),
                Some("W"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "cpu_soc_power",
                "CPU SoC Power",
                "mdi:flash-outline",
                Some("power"),
                Some("W"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_power",
                "GPU Power",
                "mdi:flash",
                Some("power"),
                Some("W"),
            )
            .await;

            // Clocks
            self.register_hwinfo_sensor(
                device,
                "cpu_effective_clock",
                "CPU Effective Clock",
                "mdi:speedometer",
                Some("frequency"),
                Some("MHz"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_core_clock",
                "GPU Core Clock",
                "mdi:speedometer",
                Some("frequency"),
                Some("MHz"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_memory_clock",
                "GPU Memory Clock",
                "mdi:speedometer-medium",
                Some("frequency"),
                Some("MHz"),
            )
            .await;

            // Utilization
            self.register_hwinfo_sensor(
                device,
                "cpu_total_usage",
                "CPU Total Usage",
                "mdi:cpu-64-bit",
                None,
                Some("%"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_core_load",
                "GPU Core Load",
                "mdi:expansion-card",
                None,
                Some("%"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "gpu_vram_usage_pct",
                "GPU VRAM Usage",
                "mdi:memory",
                None,
                Some("%"),
            )
            .await;

            // Fan + framerate
            self.register_hwinfo_sensor(
                device,
                "gpu_fan_rpm",
                "GPU Fan",
                "mdi:fan",
                None,
                Some("RPM"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "framerate",
                "Framerate",
                "mdi:speedometer",
                None,
                Some("fps"),
            )
            .await;

            // Motherboard SuperIO sensors: 4 fan headers + VRM temperature.
            self.register_hwinfo_sensor(
                device,
                "case_fan_cpu",
                "CPU Fan",
                "mdi:fan",
                None,
                Some("RPM"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "case_fan_cpu_opt",
                "CPU OPT Fan",
                "mdi:fan",
                None,
                Some("RPM"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "case_fan_system_1",
                "System Fan 1",
                "mdi:fan",
                None,
                Some("RPM"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "case_fan_system_2",
                "System Fan 2",
                "mdi:fan",
                None,
                Some("RPM"),
            )
            .await;
            self.register_hwinfo_sensor(
                device,
                "vrm_temp",
                "VRM Temperature",
                "mdi:thermometer",
                Some("temperature"),
                Some("°C"),
            )
            .await;

            // Diagnostic sensor: short summary state ("ok: 13/15 matched") +
            // rich JSON attributes (sensors_count, sample names, matched
            // keys, view_size_bytes, etc.) for remote troubleshooting when
            // HWiNFO is exposing the section but pc-bridge isn't matching
            // anything useful.
            self.register_hwinfo_sensor(
                device,
                "hwinfo_diagnostic",
                "HWiNFO Diagnostic",
                "mdi:bug-outline",
                None,
                None,
            )
            .await;
        }

        // Birth info sensor (always registered - used by Feature H birth message).
        // Uses register_sensor_with_attributes so the JSON details (os/arch/features)
        // are published to the attributes topic; state stays under HA's 255-char cap.
        self.register_sensor_with_attributes(
            device,
            "bridge_info",
            "Bridge Info",
            "mdi:information-outline",
            None,
            None,
        )
        .await;

        // Command buttons - gated by their respective features
        // Game launch button + Steam refresh
        if config.features.launch_game {
            self.register_button(device, "Launch", "mdi:rocket-launch")
                .await;
        }
        if config.features.close_game {
            self.register_button(device, "CloseGame", "mdi:close-box")
                .await;
        }
        if config.features.steam_library {
            self.register_button(device, "RefreshSteamGames", "mdi:steam")
                .await;
        }

        // Idle tracking buttons (screensaver/wake)
        if config.features.idle_tracking {
            self.register_button(device, "Screensaver", "mdi:monitor")
                .await;
            self.register_button(device, "Wake", "mdi:monitor-eye")
                .await;
        }

        // Power control buttons - each gated by its own feature flag.
        if config.features.cmd_shutdown {
            self.register_button(device, "Shutdown", "mdi:power").await;
        }
        if config.features.cmd_restart {
            self.register_button(device, "Restart", "mdi:restart").await;
        }
        if config.features.cmd_sleep {
            // Hibernate rides along with Sleep (both are suspend-to-disk/RAM).
            self.register_button(device, "Sleep", "mdi:power-sleep")
                .await;
            self.register_button(device, "Hibernate", "mdi:power-sleep")
                .await;
        }
        if config.features.cmd_lock {
            self.register_button(device, "Lock", "mdi:lock").await;
        }
        if config.features.cmd_logoff {
            self.register_button(device, "Logoff", "mdi:logout").await;
        }
        if config.features.cmd_monitor {
            self.register_button(device, "MonitorOff", "mdi:monitor-off")
                .await;
            self.register_button(device, "MonitorOn", "mdi:monitor")
                .await;
        }

        // Discord buttons
        // DiscordJoin: Expects a launcher payload like "url:discord://discord.com/channels/..."
        //   which gets expanded by expand_launcher_shortcut() and opened via Start-Process.
        // DiscordLeaveChannel: Simulates Ctrl+F6 keypress (Discord's default disconnect hotkey).
        if config.features.discord {
            self.register_button(device, "DiscordJoin", "mdi:discord")
                .await;
            self.register_button(device, "DiscordLeaveChannel", "mdi:phone-hangup")
                .await;
        }

        // Audio control commands (media keys) if enabled
        if config.features.media_controls {
            for (name, icon) in [
                ("MediaPlayPause", "mdi:play-pause"),
                ("MediaNext", "mdi:skip-next"),
                ("MediaPrevious", "mdi:skip-previous"),
                ("MediaStop", "mdi:stop"),
                ("VolumeMute", "mdi:volume-mute"),
            ] {
                self.register_button(device, name, icon).await;
            }
        }
        if config.features.volume {
            // Register volume sensor
            self.register_sensor(
                device,
                "volume_level",
                "Volume Level",
                "mdi:volume-high",
                None,
                Some("%"),
            )
            .await;
        }

        // Register notify service only if notifications enabled
        if config.features.notifications {
            self.register_notify_service(device).await;
        }

        info!("Registered HA discovery");
    }

    /// Publish an empty retained discovery config for every built-in entity
    /// whose feature is currently disabled, so a turned-off feature leaves
    /// nothing behind in Home Assistant (the entity is removed, not just marked
    /// unavailable). Idempotent: clearing an entity that was never registered
    /// is a harmless no-op (empty payload to an absent retained topic). Runs
    /// once at startup, right after `register_discovery`.
    pub(super) async fn clear_disabled_entities(&self, config: &Config) {
        let mut cleared = 0usize;
        for (component, object_id, enabled) in feature_entities(config) {
            if enabled {
                continue;
            }
            let topic = self.config_topic(component, object_id);
            if self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
                .await
                .is_ok()
            {
                cleared += 1;
            }
        }
        // The notify service uses a 3-segment device-level config topic, not the
        // per-entity shape, so clear it directly.
        if !config.features.notifications {
            let topic = format!("{}/notify/{}/config", DISCOVERY_PREFIX, self.device_name);
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
                .await;
        }
        if cleared > 0 {
            info!(
                "Cleared {} disabled HA entit{}",
                cleared,
                if cleared == 1 { "y" } else { "ies" }
            );
        }
    }

    /// Helper to register a single sensor
    async fn register_sensor(
        &self,
        device: &Arc<HADevice>,
        name: &str,
        display_name: &str,
        icon: &str,
        device_class: Option<&str>,
        unit: Option<&str>,
    ) {
        self.register_sensor_internal(device, name, display_name, icon, device_class, unit, false)
            .await;
    }

    /// Helper to register a button command
    async fn register_button(&self, device: &Arc<HADevice>, name: &str, icon: &str) {
        let payload = HADiscoveryPayload {
            name: name.to_string(),
            unique_id: format!("{}_{}", self.device_id, name),
            state_topic: None,
            command_topic: Some(self.command_topic(name)),
            availability_topic: Some(self.availability_topic()),
            availability: None,
            availability_mode: None,
            device: Arc::clone(device),
            icon: Some(icon.to_string()),
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
            json_attributes_topic: None,
        };

        let topic = self.config_topic("button", name);
        let Ok(json) = serde_json::to_string(&payload) else {
            error!("Failed to serialize HA discovery payload");
            return;
        };
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, json)
            .await;
    }

    /// Helper to register a sensor with JSON attributes support
    async fn register_sensor_with_attributes(
        &self,
        device: &Arc<HADevice>,
        name: &str,
        display_name: &str,
        icon: &str,
        device_class: Option<&str>,
        unit: Option<&str>,
    ) {
        self.register_sensor_internal(device, name, display_name, icon, device_class, unit, true)
            .await;
    }

    /// Helper to register an HWiNFO-backed sensor.
    ///
    /// HWiNFO sensors use the multi-source `availability` list so HA marks
    /// them unavailable when EITHER pc-bridge OR HWiNFO is down. The single
    /// `availability_topic` field is left empty (HA picks one or the other).
    /// `json_attributes_topic` is always set - every HWiNFO sensor publishes
    /// min/max/avg/unit attributes alongside its state.
    ///
    /// Gated to `#[cfg(windows)]` because the HWiNFO producer task is
    /// Windows-only, and the sole caller is the Windows-gated discovery block.
    #[cfg(windows)]
    async fn register_hwinfo_sensor(
        &self,
        device: &Arc<HADevice>,
        name: &str,
        display_name: &str,
        icon: &str,
        device_class: Option<&str>,
        unit: Option<&str>,
    ) {
        let availability_entries = vec![
            AvailabilityEntry {
                topic: self.availability_topic(),
            },
            AvailabilityEntry {
                topic: self.hwinfo_availability_topic(),
            },
        ];

        let payload = HADiscoveryPayload {
            name: display_name.to_string(),
            unique_id: format!("{}_{}", self.device_id, name),
            state_topic: Some(self.sensor_topic(name)),
            command_topic: None,
            availability_topic: None,
            availability: Some(availability_entries),
            availability_mode: Some("all".to_string()),
            json_attributes_topic: Some(self.sensor_attributes_topic(name)),
            device: Arc::clone(device),
            icon: Some(icon.to_string()),
            device_class: device_class.map(|s| s.to_string()),
            unit_of_measurement: unit.map(|s| s.to_string()),
            state_class: derive_state_class(device_class, unit),
        };

        let topic = self.config_topic("sensor", name);
        let Ok(json) = serde_json::to_string(&payload) else {
            error!("Failed to serialize HWiNFO HA discovery payload");
            return;
        };
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, json)
            .await;
    }

    /// Internal helper to register a sensor
    #[allow(clippy::too_many_arguments)]
    async fn register_sensor_internal(
        &self,
        device: &Arc<HADevice>,
        name: &str,
        display_name: &str,
        icon: &str,
        device_class: Option<&str>,
        unit: Option<&str>,
        with_attributes: bool,
    ) {
        let payload = HADiscoveryPayload {
            name: display_name.to_string(),
            unique_id: format!("{}_{}", self.device_id, name),
            state_topic: Some(self.sensor_topic(name)),
            command_topic: None,
            availability_topic: Some(self.availability_topic()),
            availability: None,
            availability_mode: None,
            json_attributes_topic: if with_attributes {
                Some(self.sensor_attributes_topic(name))
            } else {
                None
            },
            device: Arc::clone(device),
            icon: Some(icon.to_string()),
            device_class: device_class.map(|s| s.to_string()),
            unit_of_measurement: unit.map(|s| s.to_string()),
            state_class: derive_state_class(device_class, unit),
        };

        let topic = self.config_topic("sensor", name);
        let Ok(json) = serde_json::to_string(&payload) else {
            error!("Failed to serialize HA discovery payload");
            return;
        };
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, json)
            .await;
    }

    /// Register notify service for MQTT discovery
    async fn register_notify_service(&self, device: &Arc<HADevice>) {
        // The notify platform expects command_topic to receive messages
        let notify_topic = format!("pc-bridge/notifications/{}", self.device_name);

        let payload = serde_json::json!({
            "name": "Notification",
            "unique_id": format!("{}_notify", self.device_id),
            "command_topic": notify_topic,
            "availability_topic": self.availability_topic(),
            "device": {
                "identifiers": device.identifiers,
                "name": device.name,
                "model": device.model,
                "manufacturer": device.manufacturer,
                "sw_version": device.sw_version
            },
            "icon": "mdi:message-badge",
            "qos": 1
        });

        // notify uses 3-segment device-level config topic, not the 4-segment
        // per-entity shape - single notify service per device.
        let topic = format!("{}/notify/{}/config", DISCOVERY_PREFIX, self.device_name);
        let Ok(json) = serde_json::to_string(&payload) else {
            error!("Failed to serialize HA discovery payload");
            return;
        };
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, json)
            .await;

        debug!("Registered notify service");
    }

    /// Register custom sensors for MQTT discovery
    pub async fn register_custom_sensors(&self, sensors: &[CustomSensor]) {
        for sensor in sensors {
            let topic_name = format!("custom_{}", sensor.name);
            let display_name = format!("Custom: {}", sensor.name);
            let icon = sensor
                .icon
                .clone()
                .unwrap_or_else(|| "mdi:gauge".to_string());

            let payload = HADiscoveryPayload {
                name: display_name,
                unique_id: format!("{}_{}", self.device_id, topic_name),
                state_topic: Some(self.sensor_topic(&topic_name)),
                command_topic: None,
                availability_topic: Some(self.availability_topic()),
                availability: None,
                availability_mode: None,
                device: Arc::clone(&self.device),
                icon: Some(icon),
                device_class: None,
                unit_of_measurement: sensor.unit.clone(),
                state_class: derive_state_class(None, sensor.unit.as_deref()),
                json_attributes_topic: None,
            };

            let topic = self.config_topic("sensor", &topic_name);
            let Ok(json) = serde_json::to_string(&payload) else {
                error!("Failed to serialize HA discovery payload");
                return;
            };
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;

            debug!("Registered custom sensor: {}", sensor.name);
        }

        if !sensors.is_empty() {
            info!(
                "Registered {} custom sensor(s) for HA discovery",
                sensors.len()
            );
        }
    }

    /// Register custom commands for MQTT discovery and subscribe to their topics
    pub async fn register_custom_commands(&self, commands: &[CustomCommand]) {
        for cmd in commands {
            let icon = cmd
                .icon
                .clone()
                .unwrap_or_else(|| "mdi:console".to_string());
            let display_name = format!("Custom: {}", cmd.name);

            let payload = HADiscoveryPayload {
                name: display_name,
                unique_id: format!("{}_custom_{}", self.device_id, cmd.name),
                state_topic: None,
                command_topic: Some(self.command_topic(&cmd.name)),
                availability_topic: Some(self.availability_topic()),
                availability: None,
                availability_mode: None,
                device: Arc::clone(&self.device),
                icon: Some(icon),
                device_class: None,
                unit_of_measurement: None,
                state_class: None,
                json_attributes_topic: None,
            };

            let topic = self.config_topic("button", &cmd.name);
            let Ok(json) = serde_json::to_string(&payload) else {
                error!("Failed to serialize HA discovery payload");
                return;
            };
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;

            // Subscribe to command topic
            let cmd_topic = self.command_topic(&cmd.name);
            if let Err(e) = self.client.subscribe(&cmd_topic, QoS::AtLeastOnce).await {
                error!(
                    "Failed to subscribe to custom command {}: {:?}",
                    cmd.name, e
                );
            }

            debug!("Registered custom command: {}", cmd.name);
        }

        if !commands.is_empty() {
            info!(
                "Registered {} custom command(s) for HA discovery",
                commands.len()
            );
        }
    }
}

/// Windows-only HWiNFO sensor object ids (mirrors the `#[cfg(windows)]` HWiNFO
/// block in `register_discovery`). Listed here so teardown can clear them when
/// the HWiNFO feature is turned off.
#[cfg(windows)]
const HWINFO_ENTITY_IDS: &[&str] = &[
    "cpu_package_temp",
    "gpu_temp",
    "gpu_hotspot_temp",
    "gpu_memory_temp",
    "cpu_package_power",
    "cpu_soc_power",
    "gpu_power",
    "cpu_effective_clock",
    "gpu_core_clock",
    "gpu_memory_clock",
    "cpu_total_usage",
    "gpu_core_load",
    "gpu_vram_usage_pct",
    "gpu_fan_rpm",
    "framerate",
    "case_fan_cpu",
    "case_fan_cpu_opt",
    "case_fan_system_1",
    "case_fan_system_2",
    "vrm_temp",
    "hwinfo_diagnostic",
];

/// Every built-in HA entity the agent can register, paired with whether the
/// current config enables it. The teardown pass clears the disabled ones.
///
/// Keep in sync with `register_discovery`. A missing entry only means a stale
/// entity is not auto-removed when its feature is disabled; it never causes a
/// wrong publish. `bridge_info` is always registered, so it is intentionally
/// absent (never cleared).
fn feature_entities(config: &Config) -> Vec<(&'static str, &'static str, bool)> {
    let f = &config.features;
    // CPU, memory, and active-window share the system task that also drives the
    // battery and bridge-health sensors, so those ride along with any of them.
    let system_any = f.cpu_sensor || f.memory_sensor || f.active_window;
    #[allow(unused_mut)]
    let mut entities = vec![
        // Sensors
        ("sensor", "runninggames", f.running_game),
        ("sensor", "game_catalog", f.game_catalog),
        ("sensor", "lastactive", f.idle_tracking),
        ("sensor", "idle_seconds", f.idle_tracking),
        ("sensor", "screensaver", f.idle_tracking),
        ("sensor", "sleep_state", f.sleep_wake),
        ("sensor", "display", f.display_state),
        ("sensor", "cpu_usage", f.cpu_sensor),
        ("sensor", "memory_usage", f.memory_sensor),
        ("sensor", "active_window", f.active_window),
        ("sensor", "battery_level", system_any),
        ("sensor", "battery_charging", system_any),
        ("sensor", "bridge_health", system_any),
        ("sensor", "steam_updating", f.steam_updates),
        ("sensor", "gpu_usage", f.gpu_sensor),
        ("sensor", "network_throughput", f.network_sensor),
        ("sensor", "disk_usage", f.disk_sensor),
        ("sensor", "system_uptime", f.uptime_sensor),
        ("sensor", "volume_level", f.volume),
        // Buttons
        ("button", "Launch", f.launch_game),
        ("button", "CloseGame", f.close_game),
        ("button", "RefreshSteamGames", f.steam_library),
        ("button", "Screensaver", f.idle_tracking),
        ("button", "Wake", f.idle_tracking),
        ("button", "Shutdown", f.cmd_shutdown),
        ("button", "Restart", f.cmd_restart),
        ("button", "Sleep", f.cmd_sleep),
        ("button", "Hibernate", f.cmd_sleep),
        ("button", "Lock", f.cmd_lock),
        ("button", "Logoff", f.cmd_logoff),
        ("button", "MonitorOff", f.cmd_monitor),
        ("button", "MonitorOn", f.cmd_monitor),
        ("button", "DiscordJoin", f.discord),
        ("button", "DiscordLeaveChannel", f.discord),
        ("button", "MediaPlayPause", f.media_controls),
        ("button", "MediaNext", f.media_controls),
        ("button", "MediaPrevious", f.media_controls),
        ("button", "MediaStop", f.media_controls),
        ("button", "VolumeMute", f.media_controls),
    ];
    #[cfg(windows)]
    {
        // These sensors have Windows-only producers, so they only exist here.
        entities.push(("sensor", "session", f.session_state));
        entities.push(("sensor", "audio_device", f.audio_device));
        entities.push(("sensor", "mic", f.mic));
        entities.push(("sensor", "webcam", f.webcam));
        for oid in HWINFO_ENTITY_IDS {
            entities.push(("sensor", oid, f.hwinfo_sensor));
        }
    }
    entities
}

#[cfg(test)]
mod tests {
    use super::feature_entities;
    use crate::config::Config;

    fn enabled_of(config: &Config, component: &str, oid: &str) -> Option<bool> {
        feature_entities(config)
            .into_iter()
            .find(|(c, o, _)| *c == component && *o == oid)
            .map(|(_, _, e)| e)
    }

    #[test]
    fn default_config_keeps_power_disables_optin_sensors() {
        let config = Config::default();
        // Power features default on.
        assert_eq!(enabled_of(&config, "button", "Shutdown"), Some(true));
        assert_eq!(enabled_of(&config, "button", "Logoff"), Some(true));
        assert_eq!(enabled_of(&config, "sensor", "sleep_state"), Some(true));
        assert_eq!(enabled_of(&config, "sensor", "display"), Some(true));
        // Opt-in sensors default off, so teardown clears them.
        assert_eq!(enabled_of(&config, "sensor", "gpu_usage"), Some(false));
        assert_eq!(enabled_of(&config, "sensor", "cpu_usage"), Some(false));
        assert_eq!(enabled_of(&config, "button", "Launch"), Some(false));
    }

    #[test]
    fn cmd_sleep_gates_both_sleep_and_hibernate() {
        let mut config = Config::default();
        config.features.cmd_sleep = false;
        assert_eq!(enabled_of(&config, "button", "Sleep"), Some(false));
        assert_eq!(enabled_of(&config, "button", "Hibernate"), Some(false));
        // A different power command stays independent.
        assert_eq!(enabled_of(&config, "button", "Restart"), Some(true));
    }

    #[test]
    fn cmd_monitor_gates_both_monitor_buttons() {
        let mut config = Config::default();
        config.features.cmd_monitor = false;
        assert_eq!(enabled_of(&config, "button", "MonitorOff"), Some(false));
        assert_eq!(enabled_of(&config, "button", "MonitorOn"), Some(false));
    }

    #[test]
    fn system_sensors_share_battery_and_health() {
        let mut config = Config::default();
        config.features.cpu_sensor = true;
        assert_eq!(enabled_of(&config, "sensor", "battery_level"), Some(true));
        assert_eq!(enabled_of(&config, "sensor", "bridge_health"), Some(true));
    }
}
