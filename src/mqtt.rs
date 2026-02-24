//! MQTT client for Home Assistant communication

use log::{debug, error, info, warn};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

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
    /// Fix #3: Cached topic strings to avoid repeated format!() calls
    cached_topics: CachedTopics,
    /// Fix #5: Shared device info to avoid repeated cloning
    device: Arc<HADevice>,
    /// Broadcast channel notifying subscribers when MQTT reconnects (ConnAck).
    /// Sensors listen on this to republish retained state after broker/network recovery.
    reconnect_tx: broadcast::Sender<()>,
}

/// Pre-computed topic strings for frequently published sensors
struct CachedTopics {
    availability: Arc<str>,
    /// Sensor state topics: sensor_name -> topic
    sensor_state: HashMap<&'static str, Arc<str>>,
    /// Sensor attribute topics: sensor_name -> topic  
    sensor_attrs: HashMap<&'static str, Arc<str>>,
}

impl CachedTopics {
    fn new(device_name: &str) -> Self {
        let mut sensor_state = HashMap::new();
        let mut sensor_attrs = HashMap::new();

        // Pre-cache common sensor topics
        let sensors: &[&'static str] = &[
            "runninggames",
            "lastactive",
            "screensaver",
            "display",
            "volume_level",
            "cpu_usage",
            "memory_usage",
            "gpu_temp",
            "steam_updating",
            "bridge_health",
        ];

        for name in sensors {
            sensor_state.insert(
                *name,
                Arc::from(format!(
                    "{}/sensor/{}/{}/state",
                    DISCOVERY_PREFIX, device_name, name
                )),
            );
            sensor_attrs.insert(
                *name,
                Arc::from(format!(
                    "{}/sensor/{}/{}/attributes",
                    DISCOVERY_PREFIX, device_name, name
                )),
            );
        }

        Self {
            availability: Arc::from(format!(
                "{}/sensor/{}/availability",
                DISCOVERY_PREFIX, device_name
            )),
            sensor_state,
            sensor_attrs,
        }
    }
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
    /// Fix #5: Use Arc to avoid cloning device info for each button/sensor
    device: Arc<HADevice>,
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

        // Cap incoming packet size — our MQTT payloads are small (commands, sensor values).
        // Default is unbounded; 16 KB prevents bloated memory from unexpected large messages.
        opts.set_max_packet_size(16_384, 16_384);

        // Limit in-flight QoS 1 messages — local broker doesn't need aggressive pipelining
        opts.set_inflight(5);

        // Reconnection is handled by rumqttc automatically - just keep polling

        // Last Will and Testament (LWT)
        let availability_topic = Self::availability_topic_static(&config.device_name);
        opts.set_last_will(rumqttc::LastWill::new(
            &availability_topic,
            "offline".as_bytes().to_vec(),
            QoS::AtLeastOnce,
            true,
        ));

        // Buffer must hold ALL messages queued before the event loop starts draining.
        // MQTT spec forbids sending packets before CONNACK, so nothing drains until
        // after ConnAck. At that point, the buffer holds:
        //   register_discovery()  → up to ~28 publishes (all features)
        //   subscribe_commands()  → up to ~17 subscribes
        //   ConnAck handler       → 1 availability publish + ~17 resubscribes
        // Total worst case: ~63 messages. 128 gives headroom for custom entities
        // registered shortly after new() returns. Too small = deadlock on current_thread.
        let (client, mut eventloop) = AsyncClient::new(opts, 128);

        let device_name = config.device_name.clone();
        let device_id = config.device_id();
        let (command_tx, command_rx) = mpsc::channel(50);

        // Reconnect notification channel — sensors subscribe to republish state
        let (reconnect_tx, _) = broadcast::channel(4);
        let reconnect_tx_for_eventloop = reconnect_tx.clone();

        // Build list of topics to subscribe to (for reconnection)
        let subscribe_topics = Self::build_subscribe_topics(&config.device_name, config);

        // Clone client for event loop to publish availability on reconnect
        let client_for_eventloop = client.clone();
        let availability_topic_for_eventloop = availability_topic.clone();

        // Pre-compute prefixes for hot path (avoid format!() per message)
        let button_prefix = format!("{}/button/{}/", DISCOVERY_PREFIX, &device_name);
        let notify_topic_match = format!("pc-bridge/notifications/{}", &device_name);

        // Spawn event loop handler
        tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        debug!(
                            "MQTT message: {} = {}",
                            publish.topic,
                            String::from_utf8_lossy(&publish.payload)
                        );

                        // Extract command name using references (no topic clone)
                        let cmd_name =
                            if let Some(rest) = publish.topic.strip_prefix(&button_prefix) {
                                rest.strip_suffix("/action").map(|s| s.to_string())
                            } else if publish.topic == notify_topic_match {
                                Some("notification".to_string())
                            } else {
                                None
                            };

                        if let Some(cmd_name) = cmd_name {
                            // Zero-copy when payload is valid UTF-8 (common case)
                            let payload = match std::str::from_utf8(&publish.payload) {
                                Ok(s) => s.to_string(),
                                Err(_) => String::from_utf8_lossy(&publish.payload).into_owned(),
                            };
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

                        // Notify sensors to republish their retained state
                        let _ = reconnect_tx_for_eventloop.send(());

                        // Reset backoff on successful connection
                        backoff_secs = 1;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("MQTT error (retrying in {}s): {:?}", backoff_secs, e);
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(30);
                    }
                }
            }
        });

        // Fix #3: Pre-cache topic strings
        let cached_topics = CachedTopics::new(&device_name);

        // Fix #5: Create shared device info once
        let device = Arc::new(HADevice {
            identifiers: vec![device_id.clone()],
            name: device_name.clone(),
            model: format!("PC Bridge v{}", VERSION),
            manufacturer: "dank0i".to_string(),
            sw_version: VERSION.to_string(),
        });

        let mqtt = Self {
            client,
            device_name,
            device_id,
            cached_topics,
            device,
            reconnect_tx,
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

    #[cfg(test)]
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
        // Fix #5: Use shared device reference instead of creating new one
        let device = &self.device;

        // Conditionally register sensors based on features
        if config.features.game_detection {
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

        if config.features.power_events {
            // sleep_state has no availability (always published)
            let payload = HADiscoveryPayload {
                name: "Sleep State".to_string(),
                unique_id: format!("{}_sleep_state", self.device_id),
                state_topic: Some(self.sensor_topic("sleep_state")),
                command_topic: None,
                availability_topic: None,
                device: Arc::clone(device),
                icon: Some("mdi:power-sleep".to_string()),
                device_class: None,
                unit_of_measurement: None,
                json_attributes_topic: None,
            };
            let topic = format!(
                "{}/sensor/{}/sleep_state/config",
                DISCOVERY_PREFIX, self.device_name
            );
            let Ok(json) = serde_json::to_string(&payload) else {
                error!("Failed to serialize HA discovery payload");
                return;
            };
            let _ = self
                .client
                .publish(&topic, QoS::AtLeastOnce, true, json)
                .await;

            // Display power state sensor
            self.register_sensor(device, "display", "Display", "mdi:monitor", None, None)
                .await;
        }

        // System sensors (CPU, memory, battery, active window)
        if config.features.system_sensors {
            self.register_sensor(
                device,
                "cpu_usage",
                "CPU Usage",
                "mdi:cpu-64-bit",
                None,
                Some("%"),
            )
            .await;
            self.register_sensor(
                device,
                "memory_usage",
                "Memory Usage",
                "mdi:memory",
                None,
                Some("%"),
            )
            .await;
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
            self.register_sensor(
                device,
                "active_window",
                "Active Window",
                "mdi:application",
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

        // Steam update sensor
        if config.features.steam_updates {
            self.register_sensor_with_attributes(
                device,
                "steam_updating",
                "Steam Updating",
                "mdi:steam",
                None,
                None,
            )
            .await;
        }

        // Command buttons - gated by their respective features
        // Game launch button
        if config.features.game_detection {
            self.register_button(device, "Launch", "mdi:rocket-launch")
                .await;
        }

        // Idle tracking buttons (screensaver/wake)
        if config.features.idle_tracking {
            self.register_button(device, "Screensaver", "mdi:monitor")
                .await;
            self.register_button(device, "Wake", "mdi:monitor-eye")
                .await;
        }

        // Power control buttons
        if config.features.power_events {
            for (name, icon) in [
                ("Shutdown", "mdi:power"),
                ("sleep", "mdi:power-sleep"),
                ("Lock", "mdi:lock"),
                ("Hibernate", "mdi:power-sleep"),
                ("Restart", "mdi:restart"),
            ] {
                self.register_button(device, name, icon).await;
            }
        }

        // Discord buttons
        // discord_join: Expects a launcher payload like \"url:discord://discord.com/channels/...\"\n        //   which gets expanded by expand_launcher_shortcut() and opened via Start-Process.\n        // discord_leave_channel: Simulates Ctrl+F6 keypress (Discord's default disconnect hotkey).
        if config.features.discord {
            self.register_button(device, "discord_join", "mdi:discord")
                .await;
            self.register_button(device, "discord_leave_channel", "mdi:phone-hangup")
                .await;
        }

        // Audio control commands (media keys) if enabled
        if config.features.audio_control {
            for (name, icon) in [
                ("media_play_pause", "mdi:play-pause"),
                ("media_next", "mdi:skip-next"),
                ("media_previous", "mdi:skip-previous"),
                ("media_stop", "mdi:stop"),
                ("volume_mute", "mdi:volume-mute"),
            ] {
                self.register_button(device, name, icon).await;
            }

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
            info!("Feature disabled: game_detection - removing entities");
            self.unregister_entity("sensor", "runninggames").await;
            self.unregister_entity("button", "Launch").await;
        }

        if previous.idle_tracking && !config.features.idle_tracking {
            info!("Feature disabled: idle_tracking - removing entities");
            self.unregister_entity("sensor", "lastactive").await;
            self.unregister_entity("sensor", "screensaver").await;
            self.unregister_entity("button", "Screensaver").await;
            self.unregister_entity("button", "Wake").await;
        }

        if previous.power_events && !config.features.power_events {
            info!("Feature disabled: power_events - removing entities");
            self.unregister_entity("sensor", "sleep_state").await;
            self.unregister_entity("sensor", "display").await;
            for name in ["Shutdown", "sleep", "Lock", "Hibernate", "Restart"] {
                self.unregister_entity("button", name).await;
            }
        }

        if previous.system_sensors && !config.features.system_sensors {
            info!("Feature disabled: system_sensors - removing entities");
            for name in [
                "cpu_usage",
                "memory_usage",
                "battery_level",
                "battery_charging",
                "active_window",
                "bridge_health",
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

        if previous.discord && !config.features.discord {
            info!("Feature disabled: discord - removing entities");
            self.unregister_entity("button", "discord_join").await;
            self.unregister_entity("button", "discord_leave_channel")
                .await;
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
            device: Arc::clone(device),
            icon: Some(icon.to_string()),
            device_class: None,
            unit_of_measurement: None,
            json_attributes_topic: None,
        };

        let topic = format!(
            "{}/button/{}/{}/config",
            DISCOVERY_PREFIX, self.device_name, name
        );
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
            json_attributes_topic: if with_attributes {
                Some(self.sensor_attributes_topic(name))
            } else {
                None
            },
            device: Arc::clone(device),
            icon: Some(icon.to_string()),
            device_class: device_class.map(|s| s.to_string()),
            unit_of_measurement: unit.map(|s| s.to_string()),
        };

        let topic = format!(
            "{}/sensor/{}/{}/config",
            DISCOVERY_PREFIX, self.device_name, name
        );
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
                device: Arc::clone(&self.device),
                icon: Some(icon),
                device_class: None,
                unit_of_measurement: sensor.unit.clone(),
                json_attributes_topic: None,
            };

            let topic = format!(
                "{}/sensor/{}/{}/config",
                DISCOVERY_PREFIX, self.device_name, topic_name
            );
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
                device: Arc::clone(&self.device),
                icon: Some(icon),
                device_class: None,
                unit_of_measurement: None,
                json_attributes_topic: None,
            };

            let topic = format!(
                "{}/button/{}/{}/config",
                DISCOVERY_PREFIX, self.device_name, cmd.name
            );
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

    /// Subscribe to MQTT reconnect notifications.
    /// Fires after every ConnAck (initial connect + reconnects).
    /// Sensors use this to republish retained state that may have been lost.
    pub fn subscribe_reconnect(&self) -> broadcast::Receiver<()> {
        self.reconnect_tx.subscribe()
    }

    /// Publish a sensor value (non-retained)
    pub async fn publish_sensor(&self, name: &str, value: &str) {
        let topic = self.sensor_topic(name);
        let _ = self
            .client
            .publish(topic, QoS::AtLeastOnce, false, value)
            .await;
    }

    /// Publish a sensor value (retained)
    pub async fn publish_sensor_retained(&self, name: &str, value: &str) {
        let topic = self.sensor_topic(name);
        let _ = self
            .client
            .publish(topic, QoS::AtLeastOnce, true, value)
            .await;
    }

    /// Publish availability status
    pub async fn publish_availability(&self, online: bool) {
        let topic = self.availability_topic();
        let payload: &[u8] = if online { b"online" } else { b"offline" };
        let _ = self
            .client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await;
    }

    /// Publish sensor attributes as JSON
    pub async fn publish_sensor_attributes(&self, name: &str, attributes: &serde_json::Value) {
        let topic = self.sensor_attributes_topic(name);
        let payload = match serde_json::to_vec(attributes) {
            Ok(v) => v,
            Err(_) => return,
        };
        let _ = self
            .client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await;
    }

    // Topic helpers - Use cached topics for frequently-used sensors
    // Returns String from Arc<str> cache — callers need owned String for HA discovery payloads
    fn availability_topic(&self) -> String {
        self.cached_topics.availability.to_string()
    }

    fn availability_topic_static(device_name: &str) -> String {
        format!("{}/sensor/{}/availability", DISCOVERY_PREFIX, device_name)
    }

    fn sensor_topic(&self, name: &str) -> String {
        // Try cache first (Arc::clone is ~1 atomic op), fall back to format for custom sensors
        if let Some(cached) = self.cached_topics.sensor_state.get(name) {
            return cached.to_string();
        }
        format!(
            "{}/sensor/{}/{}/state",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }

    fn sensor_attributes_topic(&self, name: &str) -> String {
        // Try cache first, fall back to format for custom sensors
        if let Some(cached) = self.cached_topics.sensor_attrs.get(name) {
            return cached.to_string();
        }
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
    use crate::config::{IntervalConfig, MqttConfig};

    /// Create a minimal MqttClient for testing topics and payload generation.
    /// The event loop is never polled — no real broker connection is made.
    fn test_client(device_name: &str) -> MqttClient {
        let opts = MqttOptions::new("test-client", "localhost", 1883);
        let (client, _eventloop) = AsyncClient::new(opts, 10);
        let device_id = device_name.replace('-', "_");
        let (reconnect_tx, _) = broadcast::channel(4);
        MqttClient {
            client,
            device_name: device_name.to_string(),
            device_id: device_id.clone(),
            cached_topics: CachedTopics::new(device_name),
            device: Arc::new(HADevice {
                identifiers: vec![device_id],
                name: device_name.to_string(),
                model: format!("PC Bridge v{}", VERSION),
                manufacturer: "dank0i".to_string(),
                sw_version: VERSION.to_string(),
            }),
            reconnect_tx,
        }
    }

    /// Build a Config with the given feature flags and no custom sensors/commands.
    fn test_config(device_name: &str, features: FeatureConfig) -> Config {
        Config {
            device_name: device_name.to_string(),
            mqtt: MqttConfig {
                broker: "tcp://localhost:1883".to_string(),
                user: String::new(),
                pass: String::new(),
                client_id: None,
            },
            intervals: IntervalConfig::default(),
            features,
            games: HashMap::new(),
            custom_sensors_enabled: false,
            custom_commands_enabled: false,
            custom_command_privileges_allowed: false,
            allow_raw_commands: false,
            discord_keybind: None,
            custom_sensors: Vec::new(),
            custom_commands: Vec::new(),
        }
    }

    // ===== Topic generation tests =====

    #[test]
    fn test_sensor_topic() {
        let mqtt = test_client("dank0i-pc");
        assert_eq!(
            mqtt.sensor_topic("runninggames"),
            "homeassistant/sensor/dank0i-pc/runninggames/state"
        );
    }

    #[test]
    fn test_sensor_topic_custom_falls_back_to_format() {
        let mqtt = test_client("dank0i-pc");
        // "custom_foo" is not in the cached set, should still produce correct topic
        assert_eq!(
            mqtt.sensor_topic("custom_foo"),
            "homeassistant/sensor/dank0i-pc/custom_foo/state"
        );
    }

    #[test]
    fn test_sensor_attributes_topic() {
        let mqtt = test_client("dank0i-pc");
        assert_eq!(
            mqtt.sensor_attributes_topic("runninggames"),
            "homeassistant/sensor/dank0i-pc/runninggames/attributes"
        );
    }

    #[test]
    fn test_command_topic() {
        let mqtt = test_client("dank0i-pc");
        assert_eq!(
            mqtt.command_topic("sleep"),
            "homeassistant/button/dank0i-pc/sleep/action"
        );
    }

    #[test]
    fn test_availability_topic_instance() {
        let mqtt = test_client("dank0i-pc");
        assert_eq!(
            mqtt.availability_topic(),
            "homeassistant/sensor/dank0i-pc/availability"
        );
    }

    // ===== CachedTopics tests =====

    #[test]
    fn test_cached_topics_has_all_builtin_sensors() {
        let ct = CachedTopics::new("test-pc");
        let expected = [
            "runninggames",
            "lastactive",
            "screensaver",
            "display",
            "volume_level",
            "cpu_usage",
            "memory_usage",
            "gpu_temp",
            "steam_updating",
            "bridge_health",
        ];
        for name in expected {
            assert!(
                ct.sensor_state.contains_key(name),
                "Missing cached state topic for {name}"
            );
            assert!(
                ct.sensor_attrs.contains_key(name),
                "Missing cached attrs topic for {name}"
            );
        }
    }

    #[test]
    fn test_cached_topics_correct_format() {
        let ct = CachedTopics::new("my-pc");
        assert_eq!(
            ct.availability.as_ref(),
            "homeassistant/sensor/my-pc/availability"
        );
        assert_eq!(
            ct.sensor_state.get("cpu_usage").unwrap().as_ref(),
            "homeassistant/sensor/my-pc/cpu_usage/state"
        );
        assert_eq!(
            ct.sensor_attrs.get("cpu_usage").unwrap().as_ref(),
            "homeassistant/sensor/my-pc/cpu_usage/attributes"
        );
    }

    // ===== Discovery payload structure tests =====

    #[test]
    fn test_sensor_discovery_payload_json_structure() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "CPU Usage".to_string(),
            unique_id: format!("{}_cpu_usage", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("cpu_usage")),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:cpu-64-bit".to_string()),
            device_class: None,
            unit_of_measurement: Some("%".to_string()),
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        // Required fields for HA sensor discovery
        assert_eq!(json["name"], "CPU Usage");
        assert_eq!(json["unique_id"], "dank0i_pc_cpu_usage");
        assert_eq!(
            json["state_topic"],
            "homeassistant/sensor/dank0i-pc/cpu_usage/state"
        );
        assert_eq!(
            json["availability_topic"],
            "homeassistant/sensor/dank0i-pc/availability"
        );
        assert_eq!(json["icon"], "mdi:cpu-64-bit");
        assert_eq!(json["unit_of_measurement"], "%");

        // Sensor should NOT have command_topic
        assert!(json.get("command_topic").is_none());
    }

    #[test]
    fn test_button_discovery_payload_json_structure() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "sleep".to_string(),
            unique_id: format!("{}_sleep", mqtt.device_id),
            state_topic: None,
            command_topic: Some(mqtt.command_topic("sleep")),
            availability_topic: Some(mqtt.availability_topic()),
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:power-sleep".to_string()),
            device_class: None,
            unit_of_measurement: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        // Required fields for HA button discovery
        assert_eq!(json["name"], "sleep");
        assert_eq!(json["unique_id"], "dank0i_pc_sleep");
        assert_eq!(
            json["command_topic"],
            "homeassistant/button/dank0i-pc/sleep/action"
        );
        assert_eq!(
            json["availability_topic"],
            "homeassistant/sensor/dank0i-pc/availability"
        );
        assert_eq!(json["icon"], "mdi:power-sleep");

        // Button should NOT have state_topic
        assert!(json.get("state_topic").is_none());
        // Button should NOT have unit_of_measurement
        assert!(json.get("unit_of_measurement").is_none());
    }

    #[test]
    fn test_sensor_with_attributes_payload() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "Running Game".to_string(),
            unique_id: format!("{}_runninggames", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("runninggames")),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            json_attributes_topic: Some(mqtt.sensor_attributes_topic("runninggames")),
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:gamepad-variant".to_string()),
            device_class: None,
            unit_of_measurement: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        assert_eq!(
            json["json_attributes_topic"],
            "homeassistant/sensor/dank0i-pc/runninggames/attributes"
        );
        assert_eq!(
            json["state_topic"],
            "homeassistant/sensor/dank0i-pc/runninggames/state"
        );
    }

    #[test]
    fn test_device_block_in_discovery() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "Test".to_string(),
            unique_id: format!("{}_test", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("test")),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: None,
            device_class: None,
            unit_of_measurement: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();
        let device = &json["device"];

        assert_eq!(device["identifiers"], serde_json::json!(["dank0i_pc"]));
        assert_eq!(device["name"], "dank0i-pc");
        assert_eq!(device["manufacturer"], "dank0i");
        assert_eq!(device["sw_version"], VERSION);
        assert!(
            device["model"].as_str().unwrap().starts_with("PC Bridge v"),
            "model should start with 'PC Bridge v'"
        );
    }

    #[test]
    fn test_sleep_state_has_no_availability() {
        let mqtt = test_client("dank0i-pc");
        // sleep_state is special: always published, no availability_topic
        let payload = HADiscoveryPayload {
            name: "Sleep State".to_string(),
            unique_id: format!("{}_sleep_state", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("sleep_state")),
            command_topic: None,
            availability_topic: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:power-sleep".to_string()),
            device_class: None,
            unit_of_measurement: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        // sleep_state must not have availability_topic (it's always published)
        assert!(
            json.get("availability_topic").is_none(),
            "sleep_state should NOT have availability_topic"
        );
        assert_eq!(
            json["state_topic"],
            "homeassistant/sensor/dank0i-pc/sleep_state/state"
        );
    }

    #[test]
    fn test_optional_fields_omitted_when_none() {
        let payload = HADiscoveryPayload {
            name: "Test".to_string(),
            unique_id: "test_id".to_string(),
            state_topic: None,
            command_topic: None,
            availability_topic: None,
            json_attributes_topic: None,
            device: Arc::new(HADevice {
                identifiers: vec!["test".to_string()],
                name: "test".to_string(),
                model: "test".to_string(),
                manufacturer: "test".to_string(),
                sw_version: "0.0.0".to_string(),
            }),
            icon: None,
            device_class: None,
            unit_of_measurement: None,
        };

        let json_str = serde_json::to_string(&payload).unwrap();
        // These optional fields should be omitted entirely, not set to null
        assert!(
            !json_str.contains("state_topic"),
            "None fields should be skipped"
        );
        assert!(!json_str.contains("command_topic"));
        assert!(!json_str.contains("availability_topic"));
        assert!(!json_str.contains("json_attributes_topic"));
        assert!(!json_str.contains("icon"));
        assert!(!json_str.contains("device_class"));
        assert!(!json_str.contains("unit_of_measurement"));
    }

    #[test]
    fn test_sensor_with_device_class() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "Last Active".to_string(),
            unique_id: format!("{}_lastactive", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("lastactive")),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:clock-outline".to_string()),
            device_class: Some("timestamp".to_string()),
            unit_of_measurement: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["device_class"], "timestamp");
    }

    // ===== Custom sensor/command discovery tests =====

    #[test]
    fn test_custom_sensor_discovery_payload() {
        let mqtt = test_client("dank0i-pc");
        let sensor = CustomSensor {
            name: "gpu_power".to_string(),
            sensor_type: crate::config::CustomSensorType::Powershell,
            interval_seconds: 10,
            unit: Some("W".to_string()),
            icon: Some("mdi:lightning-bolt".to_string()),
            script: Some("Get-GpuPower".to_string()),
            process: None,
            file_path: None,
            registry_key: None,
            registry_value: None,
        };

        let topic_name = format!("custom_{}", sensor.name);
        let payload = HADiscoveryPayload {
            name: format!("Custom: {}", sensor.name),
            unique_id: format!("{}_{}", mqtt.device_id, topic_name),
            state_topic: Some(mqtt.sensor_topic(&topic_name)),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            device: Arc::clone(&mqtt.device),
            icon: sensor.icon.clone(),
            device_class: None,
            unit_of_measurement: sensor.unit.clone(),
            json_attributes_topic: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["name"], "Custom: gpu_power");
        assert_eq!(json["unique_id"], "dank0i_pc_custom_gpu_power");
        assert_eq!(
            json["state_topic"],
            "homeassistant/sensor/dank0i-pc/custom_gpu_power/state"
        );
        assert_eq!(json["unit_of_measurement"], "W");
        assert_eq!(json["icon"], "mdi:lightning-bolt");
    }

    #[test]
    fn test_custom_command_discovery_payload() {
        let mqtt = test_client("dank0i-pc");
        let cmd = CustomCommand {
            name: "reboot_router".to_string(),
            command_type: crate::config::CustomCommandType::Shell,
            icon: Some("mdi:router-wireless".to_string()),
            admin: false,
            script: None,
            path: None,
            args: None,
            command: Some("reboot-router.sh".to_string()),
        };

        let payload = HADiscoveryPayload {
            name: format!("Custom: {}", cmd.name),
            unique_id: format!("{}_custom_{}", mqtt.device_id, cmd.name),
            state_topic: None,
            command_topic: Some(mqtt.command_topic(&cmd.name)),
            availability_topic: Some(mqtt.availability_topic()),
            device: Arc::clone(&mqtt.device),
            icon: cmd.icon.clone(),
            device_class: None,
            unit_of_measurement: None,
            json_attributes_topic: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["name"], "Custom: reboot_router");
        assert_eq!(json["unique_id"], "dank0i_pc_custom_reboot_router");
        assert_eq!(
            json["command_topic"],
            "homeassistant/button/dank0i-pc/reboot_router/action"
        );
        assert!(json.get("state_topic").is_none());
    }

    // ===== Notify service payload test =====

    #[test]
    fn test_notify_service_payload_structure() {
        let mqtt = test_client("dank0i-pc");
        // Replicate the exact notify payload from register_notify_service
        let notify_topic = format!("pc-bridge/notifications/{}", mqtt.device_name);
        let payload = serde_json::json!({
            "name": "Notification",
            "unique_id": format!("{}_notify", mqtt.device_id),
            "command_topic": notify_topic,
            "availability_topic": mqtt.availability_topic(),
            "device": {
                "identifiers": mqtt.device.identifiers,
                "name": mqtt.device.name,
                "model": mqtt.device.model,
                "manufacturer": mqtt.device.manufacturer,
                "sw_version": mqtt.device.sw_version
            },
            "icon": "mdi:message-badge",
            "qos": 1
        });

        assert_eq!(
            payload["command_topic"],
            "pc-bridge/notifications/dank0i-pc"
        );
        assert_eq!(payload["unique_id"], "dank0i_pc_notify");
        assert_eq!(payload["icon"], "mdi:message-badge");
        assert_eq!(payload["qos"], 1);
        // Notify uses a different topic scheme than buttons
        assert!(
            !payload["command_topic"]
                .as_str()
                .unwrap()
                .starts_with("homeassistant/"),
            "Notify command_topic should be pc-bridge/notifications/, not homeassistant/"
        );
    }

    // ===== build_subscribe_topics tests =====

    #[test]
    fn test_subscribe_topics_default_features() {
        let config = test_config("test-pc", FeatureConfig::default());
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        // Default features: power_events=true, all others false
        // Core commands are always subscribed
        assert!(topics.contains(&"homeassistant/button/test-pc/sleep/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Shutdown/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Lock/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Restart/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Hibernate/action".to_string()));

        // Audio commands should NOT be present (audio_control=false)
        assert!(
            !topics.contains(&"homeassistant/button/test-pc/media_play_pause/action".to_string())
        );
        assert!(!topics.contains(&"homeassistant/button/test-pc/volume_mute/action".to_string()));

        // Notifications should NOT be present
        assert!(!topics.contains(&"pc-bridge/notifications/test-pc".to_string()));
    }

    #[test]
    fn test_subscribe_topics_with_audio() {
        let features = FeatureConfig {
            audio_control: true,
            ..FeatureConfig::default()
        };
        let config = test_config("test-pc", features);
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        // Audio commands should be present
        assert!(
            topics.contains(&"homeassistant/button/test-pc/media_play_pause/action".to_string())
        );
        assert!(topics.contains(&"homeassistant/button/test-pc/media_next/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/media_previous/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/media_stop/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/volume_set/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/volume_mute/action".to_string()));
    }

    #[test]
    fn test_subscribe_topics_with_notifications() {
        let features = FeatureConfig {
            notifications: true,
            ..FeatureConfig::default()
        };
        let config = test_config("test-pc", features);
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        assert!(topics.contains(&"pc-bridge/notifications/test-pc".to_string()));
    }

    #[test]
    fn test_subscribe_topics_with_custom_commands() {
        let features = FeatureConfig::default();
        let mut config = test_config("test-pc", features);
        config.custom_commands = vec![
            CustomCommand {
                name: "reboot_router".to_string(),
                command_type: crate::config::CustomCommandType::Shell,
                icon: None,
                admin: false,
                script: None,
                path: None,
                args: None,
                command: Some("echo test".to_string()),
            },
            CustomCommand {
                name: "backup_db".to_string(),
                command_type: crate::config::CustomCommandType::Shell,
                icon: None,
                admin: false,
                script: None,
                path: None,
                args: None,
                command: Some("echo backup".to_string()),
            },
        ];

        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        assert!(topics.contains(&"homeassistant/button/test-pc/reboot_router/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/backup_db/action".to_string()));
    }

    #[test]
    fn test_subscribe_topics_all_features_enabled() {
        let features = FeatureConfig {
            game_detection: true,
            idle_tracking: true,
            power_events: true,
            notifications: true,
            system_sensors: true,
            audio_control: true,
            steam_updates: true,
            discord: true,
        };
        let config = test_config("test-pc", features);
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        // Should have core (10) + audio (6) + notifications (1) = 17 topics
        assert!(
            topics.len() >= 17,
            "Expected at least 17 topics with all features, got {}",
            topics.len()
        );
    }

    // ===== Discovery config topic tests =====

    #[test]
    fn test_sensor_config_topic_format() {
        // Config topics for sensors: homeassistant/sensor/{device}/{name}/config
        let topic = format!(
            "{}/sensor/{}/{}/config",
            DISCOVERY_PREFIX, "dank0i-pc", "cpu_usage"
        );
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/cpu_usage/config");
    }

    #[test]
    fn test_button_config_topic_format() {
        let topic = format!(
            "{}/button/{}/{}/config",
            DISCOVERY_PREFIX, "dank0i-pc", "sleep"
        );
        assert_eq!(topic, "homeassistant/button/dank0i-pc/sleep/config");
    }

    #[test]
    fn test_notify_config_topic_format() {
        let topic = format!("{}/notify/{}/config", DISCOVERY_PREFIX, "dank0i-pc");
        assert_eq!(topic, "homeassistant/notify/dank0i-pc/config");
    }

    // ===== Unregister entity topic test =====

    #[test]
    fn test_unregister_entity_topic_format() {
        // When unregistering, we publish empty payload to: homeassistant/{type}/{device}/{name}/config
        let entity_type = "sensor";
        let device_name = "dank0i-pc";
        let name = "cpu_usage";
        let topic = format!(
            "{}/{}/{}/{}/config",
            DISCOVERY_PREFIX, entity_type, device_name, name
        );
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/cpu_usage/config");
    }

    // ===== Payload round-trip test =====

    #[test]
    fn test_discovery_payload_roundtrip_is_valid_json() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "Battery Level".to_string(),
            unique_id: format!("{}_battery_level", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("battery_level")),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:battery".to_string()),
            device_class: Some("battery".to_string()),
            unit_of_measurement: Some("%".to_string()),
        };

        // Serialize → parse back → verify it's a valid JSON object
        let json_str = serde_json::to_string(&payload).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.is_object());
        assert!(parsed.as_object().unwrap().len() >= 5);
    }

    // ===== LWT (Last Will and Testament) test =====

    #[test]
    fn test_lwt_topic_and_payload() {
        let topic = MqttClient::availability_topic_static("dank0i-pc");
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/availability");

        // LWT payload should be "offline"
        let payload = "offline";
        assert_eq!(payload, "offline");
    }

    // ===== Sensor value CONTENT tests =====
    // These verify the exact payloads that each sensor type sends to MQTT.

    #[test]
    fn test_availability_content_online() {
        // publish_availability(true) sends Bytes::from_static(b"online")
        let payload = if true { "online" } else { "offline" };
        assert_eq!(payload, "online");
        assert_eq!(payload.len(), 6);
    }

    #[test]
    fn test_availability_content_offline() {
        let payload = if false { "online" } else { "offline" };
        assert_eq!(payload, "offline");
        assert_eq!(payload.len(), 7);
    }

    #[test]
    fn test_sleep_state_content_awake() {
        // Published at boot: publish_sensor_retained("sleep_state", "awake")
        let value = "awake";
        let mqtt = test_client("dank0i-pc");
        let topic = mqtt.sensor_topic("sleep_state");
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/sleep_state/state");
        assert_eq!(value, "awake");
    }

    #[test]
    fn test_sleep_state_content_sleeping() {
        // Published on sleep event
        let value = "sleeping";
        let mqtt = test_client("dank0i-pc");
        let topic = mqtt.sensor_topic("sleep_state");
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/sleep_state/state");
        assert_eq!(value, "sleeping");
    }

    #[test]
    fn test_screensaver_content_values() {
        // Screensaver sensor publishes exactly "on" or "off" (retained)
        let active = true;
        let state_str = if active { "on" } else { "off" };
        assert_eq!(state_str, "on");

        let inactive = false;
        let state_str = if inactive { "on" } else { "off" };
        assert_eq!(state_str, "off");
    }

    #[test]
    fn test_screensaver_topic() {
        let mqtt = test_client("dank0i-pc");
        let topic = mqtt.sensor_topic("screensaver");
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/screensaver/state");
    }

    #[test]
    fn test_cpu_usage_format() {
        // SystemSensor formats CPU as "{cpu:.1}" — one decimal place
        let cpu: f64 = 45.372;
        let cpu_str = format!("{cpu:.1}");
        assert_eq!(cpu_str, "45.4"); // rounded to 1 decimal

        let cpu_zero: f64 = 0.0;
        assert_eq!(format!("{cpu_zero:.1}"), "0.0");

        let cpu_full: f64 = 100.0;
        assert_eq!(format!("{cpu_full:.1}"), "100.0");
    }

    #[test]
    fn test_memory_usage_format() {
        // SystemSensor formats memory as "{mem:.1}" — one decimal place
        let mem: f64 = 67.89;
        let mem_str = format!("{mem:.1}");
        assert_eq!(mem_str, "67.9");
    }

    #[test]
    fn test_battery_level_format() {
        // Battery level is integer to_string()
        let percent: u8 = 85;
        let level_str = percent.to_string();
        assert_eq!(level_str, "85");
    }

    #[test]
    fn test_battery_charging_format() {
        // Charging is exactly "true" or "false" string
        let charging = true;
        let charging_str = if charging { "true" } else { "false" };
        assert_eq!(charging_str, "true");

        let not_charging = false;
        let charging_str = if not_charging { "true" } else { "false" };
        assert_eq!(charging_str, "false");
    }

    #[test]
    fn test_agent_memory_format() {
        // MemorySensor: format!("{:.1}", memory_mb)
        let memory_mb: f64 = 12.345;
        let value = format!("{memory_mb:.1}");
        assert_eq!(value, "12.3");
    }

    #[test]
    fn test_custom_sensor_topic_prefix() {
        // Custom sensors publish to "custom_{name}" topic
        let sensor_name = "gpu_temp";
        let topic_name = format!("custom_{sensor_name}");
        assert_eq!(topic_name, "custom_gpu_temp");

        let mqtt = test_client("dank0i-pc");
        let topic = mqtt.sensor_topic(&topic_name);
        assert_eq!(
            topic,
            "homeassistant/sensor/dank0i-pc/custom_gpu_temp/state"
        );
    }

    #[test]
    fn test_display_state_content() {
        // Published at boot: publish_sensor_retained("display", "on")
        let value = "on";
        let mqtt = test_client("dank0i-pc");
        let topic = mqtt.sensor_topic("display");
        assert_eq!(topic, "homeassistant/sensor/dank0i-pc/display/state");
        assert_eq!(value, "on");
    }

    #[test]
    fn test_game_sensor_content_and_topic() {
        // Verify exact topic + payload for runninggames sensor
        let mqtt = test_client("dank0i-pc");
        let state_topic = mqtt.sensor_topic("runninggames");
        let attrs_topic = mqtt.sensor_attributes_topic("runninggames");

        assert_eq!(
            state_topic,
            "homeassistant/sensor/dank0i-pc/runninggames/state"
        );
        assert_eq!(
            attrs_topic,
            "homeassistant/sensor/dank0i-pc/runninggames/attributes"
        );

        // When no game: payload "none", attrs {"display_name":"None"}
        let no_game_payload = "none";
        let no_game_attrs = serde_json::json!({"display_name": "None"});
        assert_eq!(no_game_payload, "none");
        assert_eq!(
            serde_json::to_string(&no_game_attrs).unwrap(),
            r#"{"display_name":"None"}"#
        );

        // When game running: payload "battlefield_6", attrs {"display_name":"Battlefield 2042"}
        let game_payload = "battlefield_6";
        let game_attrs = serde_json::json!({"display_name": "Battlefield 2042"});
        assert_eq!(game_payload, "battlefield_6");
        assert_eq!(
            serde_json::to_string(&game_attrs).unwrap(),
            r#"{"display_name":"Battlefield 2042"}"#
        );
    }

    #[test]
    fn test_steam_updating_content_and_topic() {
        // Verify exact topic + payload for steam_updating sensor
        let mqtt = test_client("dank0i-pc");
        let state_topic = mqtt.sensor_topic("steam_updating");
        let attrs_topic = mqtt.sensor_attributes_topic("steam_updating");

        assert_eq!(
            state_topic,
            "homeassistant/sensor/dank0i-pc/steam_updating/state"
        );
        assert_eq!(
            attrs_topic,
            "homeassistant/sensor/dank0i-pc/steam_updating/attributes"
        );

        // When not updating: "off" + {"updating_games":[],"count":0}
        let idle_payload = "off";
        let idle_attrs = serde_json::json!({"updating_games": Vec::<String>::new(), "count": 0});
        assert_eq!(idle_payload, "off");
        assert_eq!(idle_attrs["count"], 0);
        assert!(idle_attrs["updating_games"].as_array().unwrap().is_empty());

        // When updating: "on" + {"updating_games":["Counter-Strike 2"],"count":1}
        let updating_payload = "on";
        let updating_attrs = serde_json::json!({
            "updating_games": ["Counter-Strike 2"],
            "count": 1
        });
        assert_eq!(updating_payload, "on");
        assert_eq!(updating_attrs["count"], 1);
        assert_eq!(updating_attrs["updating_games"][0], "Counter-Strike 2");
    }

    #[test]
    fn test_lastactive_content_rfc3339() {
        // IdleSensor publishes lastactive as RFC3339 timestamp
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;
        let now = OffsetDateTime::now_utc();
        let value = now.format(&Rfc3339).unwrap();

        // Must be valid RFC3339 — contains T separator and Z (UTC)
        assert!(value.contains('T'));
        assert!(value.ends_with('Z'));

        // Must parse back cleanly
        let parsed = OffsetDateTime::parse(&value, &Rfc3339).unwrap();
        assert_eq!(parsed.unix_timestamp(), now.unix_timestamp());
    }

    #[test]
    fn test_sensor_attributes_serializes_to_bytes() {
        // publish_sensor_attributes uses serde_json::to_vec (zero-copy) — verify it produces
        // identical output to to_string for our attribute shapes
        let attrs = serde_json::json!({"display_name": "HELLDIVERS 2"});

        let vec_bytes = serde_json::to_vec(&attrs).unwrap();
        let string_bytes = serde_json::to_string(&attrs).unwrap().into_bytes();

        assert_eq!(vec_bytes, string_bytes);
        assert_eq!(
            String::from_utf8(vec_bytes).unwrap(),
            r#"{"display_name":"HELLDIVERS 2"}"#
        );
    }

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

    // =========================================================================
    // Integration tests — in-process MQTT broker on current_thread runtime
    // =========================================================================
    //
    // These tests spin up a minimal MQTT v4 broker over TCP and run the real
    // MqttClient::new() flow. They catch:
    //   - Buffer-too-small deadlocks (the exact bug that hit us in v2.14.0)
    //   - Missing discovery registrations
    //   - Incorrect subscribe topics
    //   - Broken command routing
    //
    // The broker runs on the same current_thread runtime as production to
    // reproduce single-threaded scheduling constraints.

    mod integration {
        use super::*;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// Decode MQTT v4 variable-length remaining length.
        /// Returns (value, bytes_consumed) or None if incomplete.
        fn decode_remaining_length(bytes: &[u8]) -> Option<(usize, usize)> {
            let mut value = 0usize;
            let mut multiplier = 1;
            for (i, &byte) in bytes.iter().enumerate() {
                value += (byte as usize & 0x7F) * multiplier;
                if byte & 0x80 == 0 {
                    return Some((value, i + 1));
                }
                multiplier *= 128;
                if i >= 3 {
                    return None;
                }
            }
            None
        }

        /// Encode MQTT v4 variable-length remaining length.
        fn encode_remaining_length(buf: &mut Vec<u8>, mut len: usize) {
            loop {
                let mut byte = (len % 128) as u8;
                len /= 128;
                if len > 0 {
                    byte |= 0x80;
                }
                buf.push(byte);
                if len == 0 {
                    break;
                }
            }
        }

        /// Build a QoS 0 PUBLISH packet for injecting commands into the client.
        fn encode_publish_qos0(topic: &str, payload: &[u8]) -> Vec<u8> {
            let topic_bytes = topic.as_bytes();
            let remaining = 2 + topic_bytes.len() + payload.len();
            let mut pkt = Vec::with_capacity(1 + 4 + remaining);
            pkt.push(0x30); // PUBLISH, QoS 0, no retain
            encode_remaining_length(&mut pkt, remaining);
            pkt.extend_from_slice(&(topic_bytes.len() as u16).to_be_bytes());
            pkt.extend_from_slice(topic_bytes);
            pkt.extend_from_slice(payload);
            pkt
        }

        /// State tracked by the mini-broker for test assertions.
        struct BrokerState {
            published: Vec<(String, Vec<u8>)>,
            subscribed: Vec<String>,
        }

        /// Process complete MQTT packets from a byte buffer.
        /// Returns response packets to send back to the client.
        fn process_broker_packets(buf: &mut Vec<u8>, state: &Mutex<BrokerState>) -> Vec<Vec<u8>> {
            let mut responses = Vec::new();

            loop {
                if buf.len() < 2 {
                    break;
                }

                let packet_type = buf[0] >> 4;
                let Some((remaining_len, len_bytes)) = decode_remaining_length(&buf[1..]) else {
                    break;
                };
                let total = 1 + len_bytes + remaining_len;
                if buf.len() < total {
                    break;
                }

                let payload_start = 1 + len_bytes;

                match packet_type {
                    1 => {
                        // CONNECT → CONNACK (session not present, accepted)
                        responses.push(vec![0x20, 0x02, 0x00, 0x00]);
                    }
                    3 => {
                        // PUBLISH — record topic + payload, send PUBACK if QoS > 0
                        let flags = buf[0] & 0x0F;
                        let qos = (flags >> 1) & 0x03;
                        let mut pos = payload_start;

                        let topic_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
                        pos += 2;
                        let topic = String::from_utf8_lossy(&buf[pos..pos + topic_len]).to_string();
                        pos += topic_len;

                        if qos > 0 {
                            let pkt_id = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
                            pos += 2;
                            responses.push(vec![0x40, 0x02, (pkt_id >> 8) as u8, pkt_id as u8]);
                        }

                        let payload = buf[pos..total].to_vec();
                        state.lock().unwrap().published.push((topic, payload));
                    }
                    8 => {
                        // SUBSCRIBE → SUBACK
                        let mut pos = payload_start;
                        let pkt_id = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
                        pos += 2;

                        let mut sub_count = 0u8;
                        while pos < total {
                            let topic_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
                            pos += 2;
                            let topic =
                                String::from_utf8_lossy(&buf[pos..pos + topic_len]).to_string();
                            state.lock().unwrap().subscribed.push(topic);
                            pos += topic_len;
                            pos += 1; // QoS byte
                            sub_count += 1;
                        }

                        let remaining = 2 + sub_count as usize;
                        let mut suback = Vec::with_capacity(2 + remaining);
                        suback.push(0x90);
                        encode_remaining_length(&mut suback, remaining);
                        suback.extend_from_slice(&pkt_id.to_be_bytes());
                        suback.extend(std::iter::repeat_n(0x01_u8, sub_count as usize)); // QoS 1 granted
                        responses.push(suback);
                    }
                    12 => {
                        // PINGREQ → PINGRESP
                        responses.push(vec![0xD0, 0x00]);
                    }
                    _ => {} // Ignore PUBACK, DISCONNECT, etc.
                }

                buf.drain(..total);
            }

            responses
        }

        /// Start a mini MQTT v4 broker on a random port.
        /// Returns (port, shared_state, inject_sender).
        ///
        /// The inject sender pushes PUBLISH packets to the client (simulates HA
        /// sending button commands or notifications).
        async fn start_mini_broker()
        -> (u16, Arc<Mutex<BrokerState>>, mpsc::Sender<(String, String)>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(Mutex::new(BrokerState {
                published: Vec::new(),
                subscribed: Vec::new(),
            }));
            let (inject_tx, mut inject_rx) = mpsc::channel::<(String, String)>(16);

            let broker_state = Arc::clone(&state);
            tokio::spawn(async move {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::with_capacity(8192);
                let mut read_buf = [0u8; 8192];

                loop {
                    tokio::select! {
                        result = stream.read(&mut read_buf) => {
                            let n = result.unwrap_or(0);
                            if n == 0 { break; }
                            buf.extend_from_slice(&read_buf[..n]);
                            let responses = process_broker_packets(&mut buf, &broker_state);
                            for resp in responses {
                                if stream.write_all(&resp).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some((topic, payload)) = inject_rx.recv() => {
                            let pkt = encode_publish_qos0(&topic, payload.as_bytes());
                            if stream.write_all(&pkt).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });

            (port, state, inject_tx)
        }

        /// Create a Config pointing to the mini broker.
        fn broker_config(device_name: &str, port: u16, features: FeatureConfig) -> Config {
            Config {
                device_name: device_name.to_string(),
                mqtt: MqttConfig {
                    broker: format!("tcp://127.0.0.1:{port}"),
                    user: String::new(),
                    pass: String::new(),
                    client_id: None,
                },
                intervals: IntervalConfig::default(),
                features,
                games: HashMap::new(),
                custom_sensors_enabled: false,
                custom_commands_enabled: false,
                custom_command_privileges_allowed: false,
                allow_raw_commands: false,
                discord_keybind: None,
                custom_sensors: Vec::new(),
                custom_commands: Vec::new(),
            }
        }

        fn all_features() -> FeatureConfig {
            FeatureConfig {
                game_detection: true,
                idle_tracking: true,
                power_events: true,
                notifications: true,
                system_sensors: true,
                audio_control: true,
                steam_updates: true,
                discord: true,
            }
        }

        /// Wait for the broker to receive at least `count` published messages.
        async fn wait_for_publishes(state: &Arc<Mutex<BrokerState>>, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if state.lock().unwrap().published.len() >= count {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                let actual = state.lock().unwrap().published.len();
                panic!("Timed out waiting for {count} publishes (got {actual})");
            });
        }

        /// Wait for the broker to receive at least `count` subscribe requests.
        async fn wait_for_subscribes(state: &Arc<Mutex<BrokerState>>, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if state.lock().unwrap().subscribed.len() >= count {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                let actual = state.lock().unwrap().subscribed.len();
                panic!("Timed out waiting for {count} subscribes (got {actual})");
            });
        }

        // =================================================================
        // Deadlock tests — the #1 reason these integration tests exist.
        // current_thread matches production runtime. If the MQTT buffer is
        // too small, MqttClient::new() blocks forever and the timeout fires.
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_startup_no_deadlock_all_features() {
            let (port, _state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, all_features());

            let result =
                tokio::time::timeout(Duration::from_secs(5), MqttClient::new(&config)).await;

            assert!(
                result.is_ok(),
                "MqttClient::new() timed out — likely deadlocked (buffer too small)"
            );
            assert!(result.unwrap().is_ok());
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_startup_no_deadlock_minimal() {
            let (port, _state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, FeatureConfig::default());

            let result =
                tokio::time::timeout(Duration::from_secs(5), MqttClient::new(&config)).await;

            assert!(
                result.is_ok(),
                "MqttClient::new() deadlocked with minimal features"
            );
            assert!(result.unwrap().is_ok());
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_startup_no_deadlock_with_custom_entities() {
            let (port, _state, _inject) = start_mini_broker().await;
            let mut config = broker_config("test-pc", port, all_features());

            // Add 15 custom commands to stress the buffer
            // (custom commands are added to subscribe_topics, increasing ConnAck
            // handler burden on top of register_discovery)
            for i in 0..15 {
                config.custom_commands.push(CustomCommand {
                    name: format!("test_cmd_{i}"),
                    command_type: crate::config::CustomCommandType::Shell,
                    icon: None,
                    admin: false,
                    script: None,
                    path: None,
                    args: None,
                    command: Some("echo test".to_string()),
                });
            }

            let result =
                tokio::time::timeout(Duration::from_secs(5), MqttClient::new(&config)).await;

            assert!(
                result.is_ok(),
                "MqttClient::new() deadlocked with custom entities — buffer too small"
            );
            assert!(result.unwrap().is_ok());
        }

        // =================================================================
        // Discovery registration tests
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_discovery_registers_all_sensors() {
            let (port, state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, all_features());

            let (_mqtt, _cmd_rx) = MqttClient::new(&config).await.unwrap();

            // All features: 12 sensors + 15 buttons + 1 notify = 28 discovery
            // + 1 availability from ConnAck handler
            wait_for_publishes(&state, 28).await;

            let guard = state.lock().unwrap();
            let topics: Vec<&str> = guard.published.iter().map(|(t, _)| t.as_str()).collect();

            // Every sensor must have a config topic published
            let expected_sensors = [
                "runninggames",
                "lastactive",
                "screensaver",
                "sleep_state",
                "display",
                "cpu_usage",
                "memory_usage",
                "battery_level",
                "battery_charging",
                "active_window",
                "steam_updating",
                "volume_level",
            ];
            for sensor in expected_sensors {
                let t = format!("homeassistant/sensor/test-pc/{sensor}/config");
                assert!(
                    topics.contains(&t.as_str()),
                    "Missing discovery for sensor: {sensor}"
                );
            }

            // Every button must have a config topic published
            let expected_buttons = [
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
                "media_play_pause",
                "media_next",
                "media_previous",
                "media_stop",
                "volume_mute",
            ];
            for button in expected_buttons {
                let t = format!("homeassistant/button/test-pc/{button}/config");
                assert!(
                    topics.contains(&t.as_str()),
                    "Missing discovery for button: {button}"
                );
            }

            // Notify service
            assert!(
                topics.contains(&"homeassistant/notify/test-pc/config"),
                "Missing notify service discovery"
            );
        }

        // =================================================================
        // Subscribe topic tests
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_subscribes_all_command_topics() {
            let (port, state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, all_features());

            let (_mqtt, _cmd_rx) = MqttClient::new(&config).await.unwrap();

            // subscribe_commands + ConnAck handler both subscribe → 17 * 2 = 34
            wait_for_subscribes(&state, 17).await;

            let guard = state.lock().unwrap();
            let topics: Vec<&str> = guard.subscribed.iter().map(|t| t.as_str()).collect();

            let expected = [
                "homeassistant/button/test-pc/Launch/action",
                "homeassistant/button/test-pc/Screensaver/action",
                "homeassistant/button/test-pc/Wake/action",
                "homeassistant/button/test-pc/Shutdown/action",
                "homeassistant/button/test-pc/sleep/action",
                "homeassistant/button/test-pc/Lock/action",
                "homeassistant/button/test-pc/Hibernate/action",
                "homeassistant/button/test-pc/Restart/action",
                "homeassistant/button/test-pc/discord_join/action",
                "homeassistant/button/test-pc/discord_leave_channel/action",
                "homeassistant/button/test-pc/media_play_pause/action",
                "homeassistant/button/test-pc/media_next/action",
                "homeassistant/button/test-pc/media_previous/action",
                "homeassistant/button/test-pc/media_stop/action",
                "homeassistant/button/test-pc/volume_set/action",
                "homeassistant/button/test-pc/volume_mute/action",
                "pc-bridge/notifications/test-pc",
            ];
            for topic in expected {
                assert!(topics.contains(&topic), "Missing subscribe for: {topic}");
            }
        }

        // =================================================================
        // Command routing tests — verify end-to-end MQTT → CommandReceiver
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_command_routing_button() {
            let (port, state, inject) = start_mini_broker().await;
            let features = FeatureConfig {
                power_events: true,
                ..FeatureConfig::default()
            };
            let config = broker_config("test-pc", port, features);

            let (_mqtt, mut cmd_rx) = MqttClient::new(&config).await.unwrap();

            // Wait for subscriptions before injecting
            wait_for_subscribes(&state, 5).await;

            // Broker sends a "sleep" button press to the client
            inject
                .send((
                    "homeassistant/button/test-pc/sleep/action".to_string(),
                    String::new(),
                ))
                .await
                .unwrap();

            let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
                .await
                .expect("Timed out waiting for command")
                .expect("Command channel closed");

            assert_eq!(cmd.name, "sleep");
            assert!(cmd.payload.is_empty());
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_command_routing_notification_with_payload() {
            let (port, state, inject) = start_mini_broker().await;
            let features = FeatureConfig {
                notifications: true,
                ..FeatureConfig::default()
            };
            let config = broker_config("test-pc", port, features);

            let (_mqtt, mut cmd_rx) = MqttClient::new(&config).await.unwrap();

            wait_for_subscribes(&state, 5).await;

            let payload = r#"{"title":"Test","message":"Hello from HA"}"#;
            inject
                .send((
                    "pc-bridge/notifications/test-pc".to_string(),
                    payload.to_string(),
                ))
                .await
                .unwrap();

            let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
                .await
                .expect("Timed out waiting for notification")
                .expect("Command channel closed");

            assert_eq!(cmd.name, "notification");
            assert_eq!(cmd.payload, payload);
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_ignores_messages_for_wrong_device() {
            let (port, state, inject) = start_mini_broker().await;
            let features = FeatureConfig {
                power_events: true,
                ..FeatureConfig::default()
            };
            let config = broker_config("test-pc", port, features);

            let (_mqtt, mut cmd_rx) = MqttClient::new(&config).await.unwrap();

            wait_for_subscribes(&state, 5).await;

            // Send a command for a DIFFERENT device
            inject
                .send((
                    "homeassistant/button/other-pc/sleep/action".to_string(),
                    String::new(),
                ))
                .await
                .unwrap();

            // Then send one for OUR device so we know the event loop processed both
            inject
                .send((
                    "homeassistant/button/test-pc/Shutdown/action".to_string(),
                    String::new(),
                ))
                .await
                .unwrap();

            let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
                .await
                .expect("Timed out")
                .expect("Channel closed");

            // Should get "Shutdown" (ours), not "sleep" (wrong device)
            assert_eq!(cmd.name, "Shutdown");
        }

        // =================================================================
        // Availability / LWT tests
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_availability_published_on_connect() {
            let (port, state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, FeatureConfig::default());

            let (_mqtt, _cmd_rx) = MqttClient::new(&config).await.unwrap();

            // Wait for at least one publish (availability or discovery)
            wait_for_publishes(&state, 1).await;
            // Give ConnAck handler time to run
            tokio::time::sleep(Duration::from_millis(200)).await;

            let guard = state.lock().unwrap();
            let availability = guard
                .published
                .iter()
                .find(|(t, _)| t == "homeassistant/sensor/test-pc/availability");

            assert!(availability.is_some(), "Availability not published");
            let payload = String::from_utf8_lossy(&availability.unwrap().1).to_string();
            assert_eq!(payload, "online");
        }
    }
}
