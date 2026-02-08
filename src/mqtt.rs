//! MQTT client for Home Assistant communication

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::{Config, CustomCommand, CustomSensor, FeatureConfig};

const DISCOVERY_PREFIX: &str = "homeassistant";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Command received from Home Assistant
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub payload: String,
}

/// MQTT client wrapper
pub struct MqttClient {
    client: AsyncClient,
    device_name: String,
    device_id: String,
}

/// Receiver for commands from MQTT
pub struct CommandReceiver {
    rx: mpsc::Receiver<Command>,
}

/// Home Assistant MQTT Discovery payload
#[derive(Serialize)]
struct HADiscoveryPayload {
    name: String,
    unique_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    availability_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_attributes_topic: Option<String>,
    device: HADevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_of_measurement: Option<String>,
}

#[derive(Serialize, Clone)]
struct HADevice {
    identifiers: Vec<String>,
    name: String,
    model: String,
    manufacturer: String,
    sw_version: String,
}

impl MqttClient {
    pub async fn new(config: &Config) -> anyhow::Result<(Self, CommandReceiver)> {
        // Parse broker URL
        let broker = &config.mqtt.broker;
        let (host, port) = Self::parse_broker_url(broker)?;

        let mut opts = MqttOptions::new(config.client_id(), host, port);

        // Authentication
        if !config.mqtt.user.is_empty() {
            opts.set_credentials(&config.mqtt.user, &config.mqtt.pass);
        }

        // Connection settings
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(false); // Preserve subscriptions

        // Reconnection is handled by rumqttc automatically - just keep polling

        // Last Will and Testament (LWT)
        let availability_topic = Self::availability_topic_static(&config.device_name);
        opts.set_last_will(rumqttc::LastWill::new(
            &availability_topic,
            "offline".as_bytes().to_vec(),
            QoS::AtLeastOnce,
            true,
        ));

        let (client, mut eventloop) = AsyncClient::new(opts, 100);

        let device_name = config.device_name.clone();
        let device_id = config.device_id();
        let (command_tx, command_rx) = mpsc::channel(50);

        // Build list of topics to subscribe to (for reconnection)
        let subscribe_topics = Self::build_subscribe_topics(&config.device_name, config);

        // Clone client for event loop to publish availability on reconnect
        let client_for_eventloop = client.clone();
        let availability_topic_for_eventloop = availability_topic.clone();

        // Spawn event loop handler
        let device_name_clone = device_name.clone();
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        let topic = publish.topic.clone();
                        let payload = String::from_utf8_lossy(&publish.payload).to_string();
                        debug!("MQTT message: {} = {}", topic, payload);

                        // Extract command name from topic
                        if let Some(cmd_name) =
                            Self::extract_command_name(&topic, &device_name_clone)
                        {
                            let _ = command_tx
                                .send(Command {
                                    name: cmd_name,
                                    payload,
                                })
                                .await;
                        }
                    }
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        info!("MQTT connected - publishing availability and resubscribing");
                        // Republish availability on every connect/reconnect
                        let _ = client_for_eventloop
                            .publish(
                                &availability_topic_for_eventloop,
                                QoS::AtLeastOnce,
                                true,
                                "online",
                            )
                            .await;

                        // Re-subscribe to all command topics
                        for topic in &subscribe_topics {
                            if let Err(e) = client_for_eventloop
                                .subscribe(topic, QoS::AtLeastOnce)
                                .await
                            {
                                warn!("Failed to resubscribe to {}: {:?}", topic, e);
                            }
                        }
                        info!("Resubscribed to {} command topics", subscribe_topics.len());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("MQTT error: {:?}", e);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });

        let mqtt = Self {
            client,
            device_name,
            device_id,
        };

        let cmd_rx = CommandReceiver { rx: command_rx };

        // Register discovery and subscribe based on enabled features
        mqtt.register_discovery(config).await;
        mqtt.subscribe_commands(config).await;

        Ok((mqtt, cmd_rx))
    }

    fn parse_broker_url(url: &str) -> anyhow::Result<(String, u16)> {
        // Remove scheme prefix
        let without_scheme = url
            .strip_prefix("tcp://")
            .or_else(|| url.strip_prefix("ssl://"))
            .or_else(|| url.strip_prefix("ws://"))
            .or_else(|| url.strip_prefix("wss://"))
            .unwrap_or(url);

        let parts: Vec<&str> = without_scheme.split(':').collect();
        let host = parts.first().unwrap_or(&"localhost").to_string();
        let port = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(1883);

        Ok((host, port))
    }

    fn extract_command_name(topic: &str, device_name: &str) -> Option<String> {
        // Topic format: homeassistant/button/{device_name}/{command}/action
        let prefix = format!("{}/button/{}/", DISCOVERY_PREFIX, device_name);
        if topic.starts_with(&prefix) && topic.ends_with("/action") {
            let rest = topic.strip_prefix(&prefix)?.strip_suffix("/action")?;
            return Some(rest.to_string());
        }

        // Check notification topic: pc-bridge/notifications/{device_name}
        let notify_prefix = format!("pc-bridge/notifications/{}", device_name);
        if topic == notify_prefix {
            return Some("notification".to_string());
        }

        None
    }

    async fn register_discovery(&self, config: &Config) {
        let device = HADevice {
            identifiers: vec![self.device_id.clone()],
            name: self.device_name.clone(),
            model: format!("PC Bridge v{}", VERSION),
            manufacturer: "dank0i".to_string(),
            sw_version: VERSION.to_string(),
        };

        // Conditionally register sensors based on features
        if config.features.game_detection {
            self.register_sensor_with_attributes(
                &device,
                "runninggames",
                "Running Game",
                "mdi:gamepad-variant",
                None,
                None,
            )
            .await;
        }

        if config.features.idle_tracking {
            self.register_sensor(
                &device,
                "lastactive",
                "Last Active",
                "mdi:clock-outline",
                Some("timestamp"),
                None,
            )
            .await;
            self.register_sensor(
                &device,
                "screensaver",
                "Screensaver",
                "mdi:monitor-shimmer",
                None,
                None,
            )
            .await;
        }

        if config.features.power_events {
            // sleep_state has no availability (always published)
            let payload = HADiscoveryPayload {
                name: "Sleep State".to_string(),
                unique_id: format!("{}_sleep_state", self.device_id),
                state_topic: Some(self.sensor_topic("sleep_state")),
                command_topic: None,
                availability_topic: None,
                device: device.clone(),
                icon: Some("mdi:power-sleep".to_string()),
                device_class: None,
                unit_of_measurement: None,
                json_attributes_topic: None,
            };
            let topic = format!(
                "{}/sensor/{}/sleep_state/config",
                DISCOVERY_PREFIX, self.device_name
            );
            let json = serde_json::to_string(&payload).unwrap();
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;
        }

        // System sensors (CPU, memory, battery, active window)
        if config.features.system_sensors {
            self.register_sensor(
                &device,
                "cpu_usage",
                "CPU Usage",
                "mdi:cpu-64-bit",
                None,
                Some("%"),
            )
            .await;
            self.register_sensor(
                &device,
                "memory_usage",
                "Memory Usage",
                "mdi:memory",
                None,
                Some("%"),
            )
            .await;
            self.register_sensor(
                &device,
                "battery_level",
                "Battery Level",
                "mdi:battery",
                Some("battery"),
                Some("%"),
            )
            .await;
            self.register_sensor(
                &device,
                "battery_charging",
                "Battery Charging",
                "mdi:battery-charging",
                None,
                None,
            )
            .await;
            self.register_sensor(
                &device,
                "active_window",
                "Active Window",
                "mdi:application",
                None,
                None,
            )
            .await;
        }

        // Steam update sensor
        if config.features.steam_updates {
            self.register_sensor_with_attributes(
                &device,
                "steam_updating",
                "Steam Updating",
                "mdi:steam",
                None,
                None,
            )
            .await;
        }

        // Command buttons (always register - they're the core control interface)
        let commands = vec![
            ("Launch", "mdi:rocket-launch"),
            ("Screensaver", "mdi:monitor"),
            ("Wake", "mdi:monitor-eye"),
            ("Shutdown", "mdi:power"),
            ("sleep", "mdi:power-sleep"),
            ("Lock", "mdi:lock"),
            ("Hibernate", "mdi:power-sleep"),
            ("Restart", "mdi:restart"),
            ("discord_join", "mdi:discord"),
            ("discord_leave_channel", "mdi:phone-hangup"),
        ];

        for (name, icon) in commands {
            let payload = HADiscoveryPayload {
                name: name.to_string(),
                unique_id: format!("{}_{}", self.device_id, name),
                state_topic: None,
                command_topic: Some(self.command_topic(name)),
                availability_topic: Some(self.availability_topic()),
                device: device.clone(),
                icon: Some(icon.to_string()),
                device_class: None,
                unit_of_measurement: None,
                json_attributes_topic: None,
            };

            let topic = format!(
                "{}/button/{}/{}/config",
                DISCOVERY_PREFIX, self.device_name, name
            );
            let json = serde_json::to_string(&payload).unwrap();
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;
        }

        // Audio control commands (media keys) if enabled
        if config.features.audio_control {
            let audio_commands = vec![
                ("media_play_pause", "mdi:play-pause"),
                ("media_next", "mdi:skip-next"),
                ("media_previous", "mdi:skip-previous"),
                ("media_stop", "mdi:stop"),
                ("volume_mute", "mdi:volume-mute"),
            ];

            for (name, icon) in audio_commands {
                let payload = HADiscoveryPayload {
                    name: name.to_string(),
                    unique_id: format!("{}_{}", self.device_id, name),
                    state_topic: None,
                    command_topic: Some(self.command_topic(name)),
                    availability_topic: Some(self.availability_topic()),
                    device: device.clone(),
                    icon: Some(icon.to_string()),
                    device_class: None,
                    unit_of_measurement: None,
                    json_attributes_topic: None,
                };

                let topic = format!(
                    "{}/button/{}/{}/config",
                    DISCOVERY_PREFIX, self.device_name, name
                );
                let json = serde_json::to_string(&payload).unwrap();
                let _ = self
                    .client
                    .publish(&topic, QoS::AtLeastOnce, true, json)
                    .await;
            }

            // Register volume sensor
            self.register_sensor(
                &device,
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
            self.register_notify_service(&device).await;
        }

        // Unregister entities for features that changed from enabled → disabled
        self.unregister_changed_features(config).await;

        info!("Registered HA discovery");
    }

    /// Unregister discovery only for features that changed from enabled to disabled
    async fn unregister_changed_features(&self, config: &Config) {
        let state_path = Self::feature_state_path();
        let previous = Self::load_feature_state(&state_path);

        // Only unregister if feature was previously enabled and is now disabled
        if previous.game_detection && !config.features.game_detection {
            info!("Feature disabled: game_detection - removing entity");
            self.unregister_entity("sensor", "runninggames").await;
        }

        if previous.idle_tracking && !config.features.idle_tracking {
            info!("Feature disabled: idle_tracking - removing entity");
            self.unregister_entity("sensor", "lastactive").await;
            self.unregister_entity("sensor", "screensaver").await;
        }

        if previous.power_events && !config.features.power_events {
            info!("Feature disabled: power_events - removing entity");
            self.unregister_entity("sensor", "sleep_state").await;
        }

        if previous.system_sensors && !config.features.system_sensors {
            info!("Feature disabled: system_sensors - removing entities");
            for name in [
                "cpu_usage",
                "memory_usage",
                "battery_level",
                "battery_charging",
                "active_window",
            ] {
                self.unregister_entity("sensor", name).await;
            }
        }

        if previous.audio_control && !config.features.audio_control {
            info!("Feature disabled: audio_control - removing entities");
            for name in [
                "media_play_pause",
                "media_next",
                "media_previous",
                "media_stop",
                "volume_mute",
            ] {
                self.unregister_entity("button", name).await;
            }
            self.unregister_entity("number", "volume_set").await;
        }

        if previous.notifications && !config.features.notifications {
            info!("Feature disabled: notifications - removing entity");
            self.unregister_entity("notify", &self.device_name).await;
        }

        // Save current feature state for next comparison
        Self::save_feature_state(&state_path, &config.features);
    }

    /// Get path to feature state file (in app data dir, next to steam_cache.bin)
    fn feature_state_path() -> PathBuf {
        #[cfg(windows)]
        {
            std::env::var("LOCALAPPDATA")
                .map(|p| {
                    PathBuf::from(p)
                        .join("pc-bridge")
                        .join("feature_state.json")
                })
                .unwrap_or_else(|_| PathBuf::from("feature_state.json"))
        }
        #[cfg(unix)]
        {
            std::env::var("HOME")
                .map(|p| {
                    PathBuf::from(p)
                        .join(".cache")
                        .join("pc-bridge")
                        .join("feature_state.json")
                })
                .unwrap_or_else(|_| PathBuf::from("feature_state.json"))
        }
    }

    /// Load previous feature state (defaults to all false if not found)
    fn load_feature_state(path: &PathBuf) -> FeatureConfig {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Save current feature state
    fn save_feature_state(path: &PathBuf, features: &FeatureConfig) {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(features) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Unregister a single entity by publishing empty payload to its config topic
    async fn unregister_entity(&self, entity_type: &str, name: &str) {
        let topic = format!(
            "{}/{}/{}/{}/config",
            DISCOVERY_PREFIX, entity_type, self.device_name, name
        );
        // Empty payload removes the entity from HA
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, "")
            .await;
    }

    /// Helper to register a single sensor
    async fn register_sensor(
        &self,
        device: &HADevice,
        name: &str,
        display_name: &str,
        icon: &str,
        device_class: Option<&str>,
        unit: Option<&str>,
    ) {
        self.register_sensor_internal(device, name, display_name, icon, device_class, unit, false)
            .await;
    }

    /// Helper to register a sensor with JSON attributes support
    async fn register_sensor_with_attributes(
        &self,
        device: &HADevice,
        name: &str,
        display_name: &str,
        icon: &str,
        device_class: Option<&str>,
        unit: Option<&str>,
    ) {
        self.register_sensor_internal(device, name, display_name, icon, device_class, unit, true)
            .await;
    }

    /// Internal helper to register a sensor
    #[allow(clippy::too_many_arguments)]
    async fn register_sensor_internal(
        &self,
        device: &HADevice,
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
            json_attributes_topic: if with_attributes {
                Some(self.sensor_attributes_topic(name))
            } else {
                None
            },
            device: device.clone(),
            icon: Some(icon.to_string()),
            device_class: device_class.map(|s| s.to_string()),
            unit_of_measurement: unit.map(|s| s.to_string()),
        };

        let topic = format!(
            "{}/sensor/{}/{}/config",
            DISCOVERY_PREFIX, self.device_name, name
        );
        let json = serde_json::to_string(&payload).unwrap();
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, json)
            .await;
    }

    /// Register notify service for MQTT discovery
    async fn register_notify_service(&self, device: &HADevice) {
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

        let topic = format!("{}/notify/{}/config", DISCOVERY_PREFIX, self.device_name);
        let json = serde_json::to_string(&payload).unwrap();
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
                device: HADevice {
                    identifiers: vec![self.device_id.clone()],
                    name: self.device_name.clone(),
                    model: format!("PC Bridge v{}", VERSION),
                    manufacturer: "dank0i".to_string(),
                    sw_version: VERSION.to_string(),
                },
                icon: Some(icon),
                device_class: None,
                unit_of_measurement: sensor.unit.clone(),
                json_attributes_topic: None,
            };

            let topic = format!(
                "{}/sensor/{}/{}/config",
                DISCOVERY_PREFIX, self.device_name, topic_name
            );
            let json = serde_json::to_string(&payload).unwrap();
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
                device: HADevice {
                    identifiers: vec![self.device_id.clone()],
                    name: self.device_name.clone(),
                    model: format!("PC Bridge v{}", VERSION),
                    manufacturer: "dank0i".to_string(),
                    sw_version: VERSION.to_string(),
                },
                icon: Some(icon),
                device_class: None,
                unit_of_measurement: None,
                json_attributes_topic: None,
            };

            let topic = format!(
                "{}/button/{}/{}/config",
                DISCOVERY_PREFIX, self.device_name, cmd.name
            );
            let json = serde_json::to_string(&payload).unwrap();
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

    /// Build list of topics to subscribe to (for initial subscription and reconnection)
    fn build_subscribe_topics(device_name: &str, config: &Config) -> Vec<String> {
        let mut topics = Vec::new();

        // Core commands
        let commands = [
            "Launch",
            "Screensaver",
            "Wake",
            "Shutdown",
            "sleep",
            "Lock",
            "Hibernate",
            "Restart",
            "discord_join",
            "discord_leave_channel",
        ];

        for cmd in commands {
            topics.push(format!(
                "{}/button/{}/{}/action",
                DISCOVERY_PREFIX, device_name, cmd
            ));
        }

        // Audio commands if enabled
        if config.features.audio_control {
            let audio_commands = [
                "media_play_pause",
                "media_next",
                "media_previous",
                "media_stop",
                "volume_set",
                "volume_mute",
            ];
            for cmd in audio_commands {
                topics.push(format!(
                    "{}/button/{}/{}/action",
                    DISCOVERY_PREFIX, device_name, cmd
                ));
            }
        }

        // Notification topic if enabled
        if config.features.notifications {
            topics.push(format!("pc-bridge/notifications/{}", device_name));
        }

        // Custom commands
        for cmd in &config.custom_commands {
            topics.push(format!(
                "{}/button/{}/{}/action",
                DISCOVERY_PREFIX, device_name, cmd.name
            ));
        }

        topics
    }

    async fn subscribe_commands(&self, config: &Config) {
        let topics = Self::build_subscribe_topics(&self.device_name, config);

        for topic in &topics {
            if let Err(e) = self.client.subscribe(topic, QoS::AtLeastOnce).await {
                error!("Failed to subscribe to {}: {:?}", topic, e);
            }
        }

        info!("Subscribed to {} command topics", topics.len());
    }

    /// Publish a sensor value (non-retained)
    pub async fn publish_sensor(&self, name: &str, value: &str) {
        let topic = self.sensor_topic(name);
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, false, value)
            .await;
    }

    /// Publish a sensor value (retained)
    pub async fn publish_sensor_retained(&self, name: &str, value: &str) {
        let topic = self.sensor_topic(name);
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, value)
            .await;
    }

    /// Publish availability status
    pub async fn publish_availability(&self, online: bool) {
        let topic = self.availability_topic();
        let value = if online { "online" } else { "offline" };
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, value)
            .await;
    }

    /// Publish sensor attributes as JSON
    pub async fn publish_sensor_attributes(&self, name: &str, attributes: &serde_json::Value) {
        let topic = self.sensor_attributes_topic(name);
        let json = serde_json::to_string(attributes).unwrap_or_default();
        let _ = self
            .client
            .publish(&topic, QoS::AtLeastOnce, true, json)
            .await;
    }

    // Topic helpers
    fn availability_topic(&self) -> String {
        Self::availability_topic_static(&self.device_name)
    }

    fn availability_topic_static(device_name: &str) -> String {
        format!("{}/sensor/{}/availability", DISCOVERY_PREFIX, device_name)
    }

    fn sensor_topic(&self, name: &str) -> String {
        format!(
            "{}/sensor/{}/{}/state",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }

    fn sensor_attributes_topic(&self, name: &str) -> String {
        format!(
            "{}/sensor/{}/{}/attributes",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }

    fn command_topic(&self, name: &str) -> String {
        format!(
            "{}/button/{}/{}/action",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }
}

impl CommandReceiver {
    /// Receive next command (async)
    pub async fn recv(&mut self) -> Option<Command> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_broker_url tests =====

    #[test]
    fn test_parse_broker_url_tcp() {
        let (host, port) = MqttClient::parse_broker_url("tcp://localhost:1883").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
    }

    #[test]
    fn test_parse_broker_url_ssl() {
        let (host, port) = MqttClient::parse_broker_url("ssl://mqtt.example.com:8883").unwrap();
        assert_eq!(host, "mqtt.example.com");
        assert_eq!(port, 8883);
    }

    #[test]
    fn test_parse_broker_url_ws() {
        let (host, port) = MqttClient::parse_broker_url("ws://192.168.1.100:8083").unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 8083);
    }

    #[test]
    fn test_parse_broker_url_wss() {
        let (host, port) = MqttClient::parse_broker_url("wss://mqtt.example.com:8084").unwrap();
        assert_eq!(host, "mqtt.example.com");
        assert_eq!(port, 8084);
    }

    #[test]
    fn test_parse_broker_url_default_port() {
        let (host, port) = MqttClient::parse_broker_url("tcp://localhost").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
    }

    #[test]
    fn test_parse_broker_url_ipv4() {
        let (host, port) = MqttClient::parse_broker_url("tcp://192.168.1.1:1883").unwrap();
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 1883);
    }

    // ===== extract_command_name tests =====

    #[test]
    fn test_extract_command_name_button() {
        let topic = "homeassistant/button/dank0i-pc/sleep/action";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, Some("sleep".to_string()));
    }

    #[test]
    fn test_extract_command_name_shutdown() {
        let topic = "homeassistant/button/dank0i-pc/shutdown/action";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, Some("shutdown".to_string()));
    }

    #[test]
    fn test_extract_command_name_nested() {
        let topic = "homeassistant/button/my-pc/launch_game/action";
        let cmd = MqttClient::extract_command_name(topic, "my-pc");
        assert_eq!(cmd, Some("launch_game".to_string()));
    }

    #[test]
    fn test_extract_command_name_notification() {
        let topic = "pc-bridge/notifications/dank0i-pc";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, Some("notification".to_string()));
    }

    #[test]
    fn test_extract_command_name_wrong_device() {
        let topic = "homeassistant/button/other-pc/sleep/action";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, None);
    }

    #[test]
    fn test_extract_command_name_wrong_format() {
        let topic = "homeassistant/sensor/dank0i-pc/state";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, None);
    }

    // ===== Topic generation tests =====

    #[test]
    fn test_availability_topic_static() {
        let topic = MqttClient::availability_topic_static("test-pc");
        assert_eq!(topic, "homeassistant/sensor/test-pc/availability");
    }

    // ===== Command struct tests =====

    #[test]
    fn test_command_struct() {
        let cmd = Command {
            name: "sleep".to_string(),
            payload: "".to_string(),
        };
        assert_eq!(cmd.name, "sleep");
        assert!(cmd.payload.is_empty());
    }

    #[test]
    fn test_command_with_payload() {
        let cmd = Command {
            name: "notification".to_string(),
            payload: r#"{"title":"Test","message":"Hello"}"#.to_string(),
        };
        assert_eq!(cmd.name, "notification");
        assert!(cmd.payload.contains("Test"));
    }
}
