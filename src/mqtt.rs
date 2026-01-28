//! MQTT client for Home Assistant communication

use std::time::Duration;
use rumqttc::{AsyncClient, MqttOptions, QoS, Event, Packet};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{info, warn, error, debug};

use crate::config::Config;

const DISCOVERY_PREFIX: &str = "homeassistant";

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
    command_rx: mpsc::Receiver<Command>,
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
    device: HADevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_of_measurement: Option<String>,
}

#[derive(Serialize)]
struct HADevice {
    identifiers: Vec<String>,
    name: String,
    model: String,
    manufacturer: String,
}

impl MqttClient {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        // Parse broker URL
        let broker = &config.mqtt.broker;
        let (host, port) = Self::parse_broker_url(broker)?;

        let mut opts = MqttOptions::new(
            config.client_id(),
            host,
            port,
        );

        // Authentication
        if !config.mqtt.user.is_empty() {
            opts.set_credentials(&config.mqtt.user, &config.mqtt.pass);
        }

        // Connection settings
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(false); // Preserve subscriptions

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
                        if let Some(cmd_name) = Self::extract_command_name(&topic, &device_name_clone) {
                            let _ = command_tx.send(Command {
                                name: cmd_name,
                                payload,
                            }).await;
                        }
                    }
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        info!("MQTT connected");
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
            command_rx,
        };

        // Register discovery and subscribe
        mqtt.register_discovery().await;
        mqtt.subscribe_commands().await;

        Ok(mqtt)
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
        let host = parts.get(0).unwrap_or(&"localhost").to_string();
        let port = parts.get(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(1883);

        Ok((host, port))
    }

    fn extract_command_name(topic: &str, device_name: &str) -> Option<String> {
        // Topic format: homeassistant/button/{device_name}/{command}/action
        let prefix = format!("{}/button/{}/", DISCOVERY_PREFIX, device_name);
        if topic.starts_with(&prefix) && topic.ends_with("/action") {
            let rest = topic.strip_prefix(&prefix)?.strip_suffix("/action")?;
            return Some(rest.to_string());
        }

        // Check notification topic: hass.agent/notifications/{device_name}
        let notify_prefix = format!("hass.agent/notifications/{}", device_name);
        if topic == notify_prefix {
            return Some("notification".to_string());
        }

        None
    }

    async fn register_discovery(&self) {
        let device = HADevice {
            identifiers: vec![self.device_id.clone()],
            name: self.device_name.clone(),
            model: "PC Agent Rust".to_string(),
            manufacturer: "Custom".to_string(),
        };

        // Sensors
        let sensors = vec![
            ("runninggames", "Runninggames", "mdi:gamepad-variant", None, None),
            ("lastactive", "Last Active", "mdi:clock-outline", Some("timestamp"), None),
            ("sleep_state", "Sleep State", "mdi:power-sleep", None, None),
            ("agent_memory", "Agent Memory", "mdi:memory", None, Some("MB")),
        ];

        for (name, display_name, icon, device_class, unit) in sensors {
            let payload = HADiscoveryPayload {
                name: display_name.to_string(),
                unique_id: format!("{}_{}", self.device_id, name),
                state_topic: Some(self.sensor_topic(name)),
                command_topic: None,
                availability_topic: if name == "sleep_state" { None } else { Some(self.availability_topic()) },
                device: HADevice {
                    identifiers: vec![self.device_id.clone()],
                    name: self.device_name.clone(),
                    model: "PC Agent Rust".to_string(),
                    manufacturer: "Custom".to_string(),
                },
                icon: Some(icon.to_string()),
                device_class: device_class.map(|s| s.to_string()),
                unit_of_measurement: unit.map(|s| s.to_string()),
            };

            let topic = format!("{}/sensor/{}/{}/config", DISCOVERY_PREFIX, self.device_name, name);
            let json = serde_json::to_string(&payload).unwrap();
            let _ = self.client.publish(&topic, QoS::AtLeastOnce, true, json).await;
        }

        // Command buttons
        let commands = vec![
            ("SteamLaunch", "mdi:steam"),
            ("Screensaver", "mdi:monitor"),
            ("Wake", "mdi:monitor-eye"),
            ("Shutdown", "mdi:power"),
            ("sleep", "mdi:power-sleep"),
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
                device: HADevice {
                    identifiers: vec![self.device_id.clone()],
                    name: self.device_name.clone(),
                    model: "PC Agent Rust".to_string(),
                    manufacturer: "Custom".to_string(),
                },
                icon: Some(icon.to_string()),
                device_class: None,
                unit_of_measurement: None,
            };

            let topic = format!("{}/button/{}/{}/config", DISCOVERY_PREFIX, self.device_name, name);
            let json = serde_json::to_string(&payload).unwrap();
            let _ = self.client.publish(&topic, QoS::AtLeastOnce, true, json).await;
        }

        info!("Registered HA discovery");
    }

    async fn subscribe_commands(&self) {
        let commands = ["SteamLaunch", "Screensaver", "Wake", "Shutdown", "sleep", "discord_join", "discord_leave_channel"];
        
        for cmd in commands {
            let topic = self.command_topic(cmd);
            if let Err(e) = self.client.subscribe(&topic, QoS::AtLeastOnce).await {
                error!("Failed to subscribe to {}: {:?}", topic, e);
            }
        }

        // Notification topic
        let notify_topic = format!("hass.agent/notifications/{}", self.device_name);
        let _ = self.client.subscribe(&notify_topic, QoS::AtLeastOnce).await;

        info!("Subscribed to command topics");
    }

    /// Publish a sensor value (non-retained)
    pub async fn publish_sensor(&self, name: &str, value: &str) {
        let topic = self.sensor_topic(name);
        let _ = self.client.publish(&topic, QoS::AtLeastOnce, false, value).await;
    }

    /// Publish a sensor value (retained)
    pub async fn publish_sensor_retained(&self, name: &str, value: &str) {
        let topic = self.sensor_topic(name);
        let _ = self.client.publish(&topic, QoS::AtLeastOnce, true, value).await;
    }

    /// Publish availability status
    pub async fn publish_availability(&self, online: bool) {
        let topic = self.availability_topic();
        let value = if online { "online" } else { "offline" };
        let _ = self.client.publish(&topic, QoS::AtLeastOnce, true, value).await;
    }

    /// Receive next command (async)
    pub async fn recv_command(&mut self) -> Option<Command> {
        self.command_rx.recv().await
    }

    // Topic helpers
    fn availability_topic(&self) -> String {
        Self::availability_topic_static(&self.device_name)
    }

    fn availability_topic_static(device_name: &str) -> String {
        format!("{}/sensor/{}/availability", DISCOVERY_PREFIX, device_name)
    }

    fn sensor_topic(&self, name: &str) -> String {
        format!("{}/sensor/{}/{}/state", DISCOVERY_PREFIX, self.device_name, name)
    }

    fn command_topic(&self, name: &str) -> String {
        format!("{}/button/{}/{}/action", DISCOVERY_PREFIX, self.device_name, name)
    }
}
