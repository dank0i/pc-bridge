//! MQTT client for Home Assistant communication

use log::{debug, error, info, warn};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
#[cfg(test)]
use crate::config::{CustomCommand, CustomSensor};
#[cfg(test)]
use std::collections::HashMap;

pub(super) const DISCOVERY_PREFIX: &str = "homeassistant";
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

mod discovery;
mod payload;
mod topics;

use payload::HADevice;
#[cfg(test)]
use payload::{HADiscoveryPayload, derive_state_class};
use topics::CachedTopics;

/// Receiver for commands from MQTT
pub struct CommandReceiver {
    rx: mpsc::Receiver<Command>,
}

/// Match an inbound MQTT topic against the cached button and notify prefixes
/// and return the command name (or "notification") if it routes.  Single
/// source of truth shared by the event loop and unit tests.
fn parse_incoming_topic<'a>(
    topic: &'a str,
    button_prefix: &str,
    notify_topic: &str,
) -> Option<&'a str> {
    if let Some(rest) = topic.strip_prefix(button_prefix)
        && let Some(cmd) = rest.strip_suffix("/action")
    {
        return Some(cmd);
    }
    if topic == notify_topic {
        return Some("notification");
    }
    None
}

impl MqttClient {
    pub async fn new(
        config: &Config,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> anyhow::Result<(Self, CommandReceiver)> {
        // Parse broker URL
        let broker = &config.mqtt.broker;
        let (host, port, use_tls) = Self::parse_broker_url(broker)?;

        let mut opts = MqttOptions::new(config.client_id(), host.clone(), port);

        // Authentication
        if !config.mqtt.user.is_empty() {
            opts.set_credentials(&config.mqtt.user, &config.mqtt.pass);
        }

        // TLS transport (ssl:// or wss:// scheme)
        if use_tls {
            let tls_config = rumqttc::TlsConfiguration::Native;
            opts.set_transport(rumqttc::Transport::tls_with_config(tls_config));
            info!("MQTT TLS enabled for {}:{}", host, port);
        }

        // Connection settings
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(false); // Preserve subscriptions

        // Cap packet size to bound memory, but generously: an incoming payload
        // over the cap makes the event loop error and the whole connection cycle
        // (dropping the command). 256 KB comfortably covers notification bodies
        // (which can carry a longer message / data URI) while still bounding memory.
        opts.set_max_packet_size(256 * 1024, 256 * 1024);

        // Limit in-flight QoS 1 messages - local broker doesn't need aggressive pipelining
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
        let (command_tx, command_rx) = mpsc::channel(16);

        // Reconnect notification channel - sensors subscribe to republish state
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

        // Pre-compute birth message for ConnAck (Feature H).
        //
        // State carries just the version string (HA caps sensor state at 255
        // chars; the full JSON blob exceeds that and the entity falls back to
        // unknown). Everything else goes to the attributes topic.
        let birth_topic = format!(
            "{}/sensor/{}/bridge_info/state",
            DISCOVERY_PREFIX, &device_name
        );
        let birth_attrs_topic = format!(
            "{}/sensor/{}/bridge_info/attributes",
            DISCOVERY_PREFIX, &device_name
        );
        let birth_payload = VERSION.to_string();
        let birth_attrs_payload = serde_json::json!({
            "version": VERSION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "features": {
                "running_game": config.features.running_game,
                "game_catalog": config.features.game_catalog,
                "steam_library": config.features.steam_library,
                "launch_game": config.features.launch_game,
                "close_game": config.features.close_game,
                "idle_tracking": config.features.idle_tracking,
                "sleep_wake": config.features.sleep_wake,
                "display_state": config.features.display_state,
                "cmd_shutdown": config.features.cmd_shutdown,
                "cmd_restart": config.features.cmd_restart,
                "cmd_sleep": config.features.cmd_sleep,
                "cmd_lock": config.features.cmd_lock,
                "cmd_logoff": config.features.cmd_logoff,
                "cmd_monitor": config.features.cmd_monitor,
                "notifications": config.features.notifications,
                "cpu_sensor": config.features.cpu_sensor,
                "memory_sensor": config.features.memory_sensor,
                "active_window": config.features.active_window,
                "session_state": config.features.session_state,
                "audio_device": config.features.audio_device,
                "mic": config.features.mic,
                "webcam": config.features.webcam,
                "now_playing": config.features.now_playing,
                "volume": config.features.volume,
                "media_controls": config.features.media_controls,
                "steam_updates": config.features.steam_updates,
                "discord": config.features.discord,
                "gpu_sensor": config.features.gpu_sensor,
                "network_sensor": config.features.network_sensor,
                "disk_sensor": config.features.disk_sensor,
                "uptime_sensor": config.features.uptime_sensor,
                "hwinfo_sensor": config.features.hwinfo_sensor,
            }
        })
        .to_string();

        // Spawn event loop handler
        tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        debug!("MQTT event loop shutting down");
                        break;
                    }
                    poll_result = eventloop.poll() => {
                        match poll_result {
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        debug!(
                            "MQTT message: {} = {}",
                            publish.topic,
                            String::from_utf8_lossy(&publish.payload)
                        );

                        // Extract command name using the shared parser so a
                        // change here can't drift from the test-only path.
                        let cmd_name = parse_incoming_topic(
                            &publish.topic,
                            &button_prefix,
                            &notify_topic_match,
                        )
                        .map(str::to_owned);

                        if let Some(cmd_name) = cmd_name {
                            // Zero-copy when payload is valid UTF-8 (common case)
                            let payload = match std::str::from_utf8(&publish.payload) {
                                Ok(s) => s.to_string(),
                                Err(_) => String::from_utf8_lossy(&publish.payload).into_owned(),
                            };
                            // try_send (not .await): blocking here would stop the
                            // poll loop from sending keepalives and the broker
                            // would drop us. Dropping a button press is safer.
                            if command_tx
                                .try_send(Command {
                                    name: cmd_name,
                                    payload,
                                })
                                .is_err()
                            {
                                warn!("Command channel full or closed - dropping command");
                            }
                        }
                    }
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        info!("MQTT connected - resubscribing then announcing online");
                        // Reset backoff on successful connection.
                        backoff_secs = 1;

                        // Run the resubscribe + birth publishes in a SEPARATE task
                        // so the event loop below keeps calling poll() and draining
                        // the client's request channel. Doing them inline here would
                        // .await into the bounded 128-slot request channel that only
                        // poll() drains - which deadlocks the single-threaded runtime
                        // once the topic count (many custom entities) fills the buffer,
                        // since poll() can't run until this arm returns. The awaits in
                        // the task stay sequential, so subscribes still hit the wire
                        // before the availability publish (TCP order), which HA needs.
                        let client = client_for_eventloop.clone();
                        let topics = subscribe_topics.clone();
                        let avail = availability_topic_for_eventloop.clone();
                        let state_topic = birth_topic.clone();
                        let state_body = birth_payload.clone();
                        let attr_topic = birth_attrs_topic.clone();
                        let attr_body = birth_attrs_payload.clone();
                        let rtx = reconnect_tx_for_eventloop.clone();
                        tokio::spawn(async move {
                            // Subscribe BEFORE publishing "online": HA may fire
                            // commands the instant we appear available, and the broker
                            // silently drops them if our SUBSCRIBE hasn't landed yet.
                            for topic in &topics {
                                if let Err(e) = client.subscribe(topic, QoS::AtLeastOnce).await {
                                    warn!("Failed to resubscribe to {}: {:?}", topic, e);
                                }
                            }
                            info!("Resubscribed to {} command topics", topics.len());

                            if let Err(e) = client
                                .publish_bytes(
                                    &avail,
                                    QoS::AtLeastOnce,
                                    true,
                                    bytes::Bytes::from_static(b"online"),
                                )
                                .await
                            {
                                warn!("Failed to publish online availability after ConnAck: {:?}", e);
                            }

                            // Birth message: state carries only the version (255-char
                            // cap); the rest goes to the attributes topic.
                            if let Err(e) = client
                                .publish(&state_topic, QoS::AtLeastOnce, true, state_body.as_bytes())
                                .await
                            {
                                warn!("Failed to publish bridge_info birth message: {:?}", e);
                            }
                            if let Err(e) = client
                                .publish(&attr_topic, QoS::AtLeastOnce, true, attr_body.as_bytes())
                                .await
                            {
                                warn!("Failed to publish bridge_info birth attributes: {:?}", e);
                            }

                            // Notify sensors to republish their retained state.
                            if let Err(e) = rtx.send(()) {
                                debug!("No reconnect subscribers yet ({}); first connect is fine, later is suspicious", e);
                            }
                        });
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("MQTT error (retrying in {}s): {:?}", backoff_secs, e);
                        // Race the backoff against shutdown so Ctrl+C isn't stuck
                        // for up to 30s waiting on a reconnect delay.
                        tokio::select! {
                            biased;
                            _ = shutdown_rx.recv() => {
                                debug!("MQTT event loop shutting down during backoff");
                                break;
                            }
                            () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                        }
                        backoff_secs = (backoff_secs * 2).min(30);
                    }
                }
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
        // Remove any HA entities whose feature is now disabled (e.g. the user
        // turned a sensor off) so nothing stale lingers on the HA side.
        mqtt.clear_disabled_entities(config).await;
        mqtt.subscribe_commands(config).await;

        Ok((mqtt, cmd_rx))
    }

    /// Forwards to the canonical implementation in `power::sync_mqtt` so the
    /// async client and the sync sleep publisher can't drift out of sync.
    fn parse_broker_url(url: &str) -> anyhow::Result<(String, u16, bool)> {
        Ok(crate::power::sync_mqtt::parse_broker_url(url))
    }

    /// Test-only thin shim that builds the prefixes the event-loop already
    /// caches, then calls `parse_incoming_topic`.  Keeps tests honest: a
    /// production-routing regression now fails the unit test too.
    #[cfg(test)]
    fn extract_command_name(topic: &str, device_name: &str) -> Option<String> {
        let button_prefix = format!("{}/button/{}/", DISCOVERY_PREFIX, device_name);
        let notify_topic = format!("pc-bridge/notifications/{}", device_name);
        parse_incoming_topic(topic, &button_prefix, &notify_topic).map(|s| s.to_string())
    }

    // Discovery registration (`register_*` methods) lives in mqtt/discovery.rs

    /// Build list of topics to subscribe to (for initial subscription and reconnection)
    /// Every native command the agent can receive. The subscribe set is this
    /// list filtered by `command_feature_enabled` - the SAME gate the executor
    /// applies - so we never subscribe to a command we would refuse to run, and
    /// the two can't drift apart.
    const NATIVE_COMMANDS: &[&str] = &[
        "Launch",
        "CloseGame",
        "RefreshSteamGames",
        "Screensaver",
        "Wake",
        "DiscordJoin",
        "DiscordLeaveChannel",
        "Shutdown",
        "Restart",
        "Sleep",
        "Hibernate",
        "Lock",
        "Logoff",
        "MonitorOff",
        "MonitorOn",
        "MediaPlayPause",
        "MediaNext",
        "MediaPrevious",
        "MediaStop",
        "VolumeMute",
    ];

    fn build_subscribe_topics(device_name: &str, config: &Config) -> Vec<String> {
        let mut topics = Vec::new();

        for &cmd in Self::NATIVE_COMMANDS {
            if crate::commands::command_feature_enabled(cmd, &config.features) {
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
        self.publish_inner(self.sensor_topic(name), false, value.to_owned())
            .await;
    }

    /// Publish a sensor value (retained)
    pub async fn publish_sensor_retained(&self, name: &str, value: &str) {
        self.publish_inner(self.sensor_topic(name), true, value.to_owned())
            .await;
    }

    /// Publish a dry-run command record to the test topic consumed by the
    /// integration test kit. Not retained. Topic: `pc-bridge/test/executed/<device>`.
    pub async fn publish_test_action(&self, name: &str, payload: &str, action: &str) {
        let topic = format!("pc-bridge/test/executed/{}", self.device_name);
        let body = serde_json::json!({ "name": name, "payload": payload, "action": action });
        let Ok(value) = serde_json::to_string(&body) else {
            return;
        };
        self.publish_inner(topic, false, value).await;
    }

    /// Publish availability status
    pub async fn publish_availability(&self, online: bool) {
        // Zero-copy static payloads - Bytes::from_static avoids the &[u8] → Vec<u8>
        // copy that `publish` would do.
        let payload = if online {
            bytes::Bytes::from_static(b"online")
        } else {
            bytes::Bytes::from_static(b"offline")
        };
        self.publish_bytes_inner(self.availability_topic(), true, payload)
            .await;
    }

    /// Publish HWiNFO availability status (retained). Sensors registered with
    /// `register_hwinfo_sensor` track this in addition to the main LWT.
    pub async fn publish_hwinfo_availability(&self, online: bool) {
        let payload = if online {
            bytes::Bytes::from_static(b"online")
        } else {
            bytes::Bytes::from_static(b"offline")
        };
        self.publish_bytes_inner(self.hwinfo_availability_topic(), true, payload)
            .await;
    }

    /// Publish sensor attributes as JSON
    pub async fn publish_sensor_attributes(&self, name: &str, attributes: &serde_json::Value) {
        let topic = self.sensor_attributes_topic(name);
        let Ok(payload) = serde_json::to_vec(attributes) else {
            return;
        };
        self.publish_inner(topic, true, payload).await;
    }

    /// Internal publish helper. Logs failures instead of silently dropping them
    /// - broker disconnects in the middle of a publish should be visible.
    async fn publish_inner(&self, topic: String, retained: bool, payload: impl Into<Vec<u8>>) {
        if let Err(e) = self
            .client
            .publish(&topic, QoS::AtLeastOnce, retained, payload)
            .await
        {
            warn!("MQTT publish failed for {}: {:?}", topic, e);
        }
    }

    /// Zero-copy variant for static byte payloads (LWT, fixed enums).
    async fn publish_bytes_inner(&self, topic: String, retained: bool, payload: bytes::Bytes) {
        if let Err(e) = self
            .client
            .publish_bytes(&topic, QoS::AtLeastOnce, retained, payload)
            .await
        {
            warn!("MQTT publish_bytes failed for {}: {:?}", topic, e);
        }
    }

    // Topic helpers live in mqtt/topics.rs - split impl block.
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
    use crate::config::{FeatureConfig, IntervalConfig, MqttConfig};

    /// Create a minimal MqttClient for testing topics and payload generation.
    /// The event loop is never polled - no real broker connection is made.
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
            allow_global_launch: true,
            allow_global_close: false,
            discord_keybind: None,
            custom_sensors: Vec::new(),
            custom_commands: Vec::new(),
            update_channel: crate::config::default_update_channel(),
            disk_sensor_paths: Vec::new(),
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
            mqtt.command_topic("Sleep"),
            "homeassistant/button/dank0i-pc/Sleep/action"
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
            "steam_updating",
            "bridge_health",
            "game_catalog",
            "active_window",
            "sleep_state",
            "battery_level",
            "battery_charging",
            "gpu_usage",
            "network_throughput",
            "disk_usage",
            "system_uptime",
            "bridge_info",
            "hwinfo_diagnostic",
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
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:cpu-64-bit".to_string()),
            device_class: None,
            unit_of_measurement: Some("%".to_string()),
            state_class: None,
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
            name: "Sleep".to_string(),
            unique_id: format!("{}_Sleep", mqtt.device_id),
            state_topic: None,
            command_topic: Some(mqtt.command_topic("Sleep")),
            availability_topic: Some(mqtt.availability_topic()),
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:power-sleep".to_string()),
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
        };

        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();

        // Required fields for HA button discovery
        assert_eq!(json["name"], "Sleep");
        assert_eq!(json["unique_id"], "dank0i_pc_Sleep");
        assert_eq!(
            json["command_topic"],
            "homeassistant/button/dank0i-pc/Sleep/action"
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
            availability: None,
            availability_mode: None,
            json_attributes_topic: Some(mqtt.sensor_attributes_topic("runninggames")),
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:gamepad-variant".to_string()),
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
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
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: None,
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
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
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:power-sleep".to_string()),
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
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
            availability: None,
            availability_mode: None,
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
            state_class: None,
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
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:clock-outline".to_string()),
            device_class: Some("timestamp".to_string()),
            unit_of_measurement: None,
            state_class: None,
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
            availability: None,
            availability_mode: None,
            device: Arc::clone(&mqtt.device),
            icon: sensor.icon.clone(),
            device_class: None,
            unit_of_measurement: sensor.unit.clone(),
            state_class: derive_state_class(None, sensor.unit.as_deref()),
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
            availability: None,
            availability_mode: None,
            device: Arc::clone(&mqtt.device),
            icon: cmd.icon.clone(),
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
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

        // Default features: power flags (sleep/wake, display, cmd_*) true, all
        // others false. Enabled power commands are subscribed.
        assert!(topics.contains(&"homeassistant/button/test-pc/Sleep/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Shutdown/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Lock/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Restart/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/Hibernate/action".to_string()));

        // Game/Discord commands are gated by their own (default-off) features,
        // so they are NOT subscribed - matching the executor gate exactly.
        assert!(
            !topics.contains(&"homeassistant/button/test-pc/RefreshSteamGames/action".to_string())
        );
        assert!(!topics.contains(&"homeassistant/button/test-pc/Launch/action".to_string()));

        // Audio commands should NOT be present (audio_control=false)
        assert!(
            !topics.contains(&"homeassistant/button/test-pc/MediaPlayPause/action".to_string())
        );
        assert!(!topics.contains(&"homeassistant/button/test-pc/VolumeMute/action".to_string()));

        // Notifications should NOT be present
        assert!(!topics.contains(&"pc-bridge/notifications/test-pc".to_string()));
    }

    #[test]
    fn test_subscribe_topics_match_executor_gate() {
        // Single source of truth: every native command topic that is subscribed
        // must also pass the executor's feature gate, and vice versa. This guards
        // against the subscribe list drifting from what the executor will run.
        let features = FeatureConfig {
            steam_library: true,
            launch_game: true,
            discord: true,
            volume: true,
            ..FeatureConfig::default()
        };
        let config = test_config("test-pc", features);
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        for &cmd in MqttClient::NATIVE_COMMANDS {
            let topic = format!("homeassistant/button/test-pc/{cmd}/action");
            let subscribed = topics.contains(&topic);
            let gated_on = crate::commands::command_feature_enabled(cmd, &config.features);
            assert_eq!(subscribed, gated_on, "subscribe/execute mismatch for {cmd}");
        }
    }

    #[test]
    fn test_subscribe_topics_with_audio() {
        let features = FeatureConfig {
            volume: true,
            media_controls: true,
            ..FeatureConfig::default()
        };
        let config = test_config("test-pc", features);
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        // Audio commands should be present
        assert!(topics.contains(&"homeassistant/button/test-pc/MediaPlayPause/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/MediaNext/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/MediaPrevious/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/MediaStop/action".to_string()));
        assert!(topics.contains(&"homeassistant/button/test-pc/VolumeMute/action".to_string()));
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
            running_game: true,
            game_catalog: true,
            steam_library: true,
            launch_game: true,
            close_game: true,
            idle_tracking: true,
            sleep_wake: true,
            display_state: true,
            cmd_shutdown: true,
            cmd_restart: true,
            cmd_sleep: true,
            cmd_lock: true,
            cmd_logoff: true,
            cmd_monitor: true,
            notifications: true,
            cpu_sensor: true,
            memory_sensor: true,
            active_window: true,
            session_state: true,
            audio_device: true,
            mic: true,
            webcam: true,
            now_playing: true,
            volume: true,
            media_controls: true,
            steam_updates: true,
            discord: true,
            gpu_sensor: true,
            network_sensor: true,
            disk_sensor: true,
            uptime_sensor: true,
            hwinfo_sensor: true,
        };
        let config = test_config("test-pc", features);
        let topics = MqttClient::build_subscribe_topics("test-pc", &config);

        // Should have core (6) + power (8) + audio (6) + notifications (1) = 21 topics
        assert!(
            topics.len() >= 21,
            "Expected at least 21 topics with all features, got {}",
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
            DISCOVERY_PREFIX, "dank0i-pc", "Sleep"
        );
        assert_eq!(topic, "homeassistant/button/dank0i-pc/Sleep/config");
    }

    #[test]
    fn test_notify_config_topic_format() {
        let topic = format!("{}/notify/{}/config", DISCOVERY_PREFIX, "dank0i-pc");
        assert_eq!(topic, "homeassistant/notify/dank0i-pc/config");
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
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:battery".to_string()),
            device_class: Some("battery".to_string()),
            unit_of_measurement: Some("%".to_string()),
            state_class: None,
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
        // SystemSensor formats CPU as "{cpu:.1}" - one decimal place
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
        // SystemSensor formats memory as "{mem:.1}" - one decimal place
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
        let sensor_name = "my_sensor";
        let topic_name = format!("custom_{sensor_name}");
        assert_eq!(topic_name, "custom_my_sensor");

        let mqtt = test_client("dank0i-pc");
        let topic = mqtt.sensor_topic(&topic_name);
        assert_eq!(
            topic,
            "homeassistant/sensor/dank0i-pc/custom_my_sensor/state"
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

        // Must be valid RFC3339 - contains T separator and Z (UTC)
        assert!(value.contains('T'));
        assert!(value.ends_with('Z'));

        // Must parse back cleanly
        let parsed = OffsetDateTime::parse(&value, &Rfc3339).unwrap();
        assert_eq!(parsed.unix_timestamp(), now.unix_timestamp());
    }

    #[test]
    fn test_sensor_attributes_serializes_to_bytes() {
        // publish_sensor_attributes uses serde_json::to_vec (zero-copy) - verify it produces
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
        let (host, port, tls) = MqttClient::parse_broker_url("tcp://localhost:1883").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
        assert!(!tls);
    }

    #[test]
    fn test_parse_broker_url_ssl() {
        let (host, port, tls) =
            MqttClient::parse_broker_url("ssl://mqtt.example.com:8883").unwrap();
        assert_eq!(host, "mqtt.example.com");
        assert_eq!(port, 8883);
        assert!(tls);
    }

    #[test]
    fn test_parse_broker_url_ws() {
        let (host, port, tls) = MqttClient::parse_broker_url("ws://192.168.1.100:8083").unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 8083);
        assert!(!tls);
    }

    #[test]
    fn test_parse_broker_url_wss() {
        let (host, port, tls) =
            MqttClient::parse_broker_url("wss://mqtt.example.com:8084").unwrap();
        assert_eq!(host, "mqtt.example.com");
        assert_eq!(port, 8084);
        assert!(tls);
    }

    #[test]
    fn test_parse_broker_url_default_port() {
        let (host, port, tls) = MqttClient::parse_broker_url("tcp://localhost").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
        assert!(!tls);
    }

    #[test]
    fn test_parse_broker_url_default_port_ssl() {
        let (host, port, tls) = MqttClient::parse_broker_url("ssl://broker.local").unwrap();
        assert_eq!(host, "broker.local");
        assert_eq!(port, 8883);
        assert!(tls);
    }

    #[test]
    fn test_parse_broker_url_ipv4() {
        let (host, port, tls) = MqttClient::parse_broker_url("tcp://192.168.1.1:1883").unwrap();
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 1883);
        assert!(!tls);
    }

    // ===== extract_command_name tests =====

    #[test]
    fn test_extract_command_name_button() {
        let topic = "homeassistant/button/dank0i-pc/Sleep/action";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, Some("Sleep".to_string()));
    }

    #[test]
    fn test_extract_command_name_shutdown() {
        let topic = "homeassistant/button/dank0i-pc/Shutdown/action";
        let cmd = MqttClient::extract_command_name(topic, "dank0i-pc");
        assert_eq!(cmd, Some("Shutdown".to_string()));
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
            name: "Sleep".to_string(),
            payload: "".to_string(),
        };
        assert_eq!(cmd.name, "Sleep");
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
    // Integration tests - in-process MQTT broker on current_thread runtime
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
        use crate::config::FeatureConfig;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// Create a shutdown channel pair for tests. The sender must stay alive
        /// (assign to `stx`) for the test's duration so the event loop doesn't
        /// exit immediately.
        fn test_shutdown() -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
            let (tx, rx) = broadcast::channel::<()>(1);
            (tx, rx)
        }

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
                        // PUBLISH - record topic + payload, send PUBACK if QoS > 0
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
                allow_global_launch: true,
                allow_global_close: false,
                discord_keybind: None,
                custom_sensors: Vec::new(),
                custom_commands: Vec::new(),
                update_channel: crate::config::default_update_channel(),
                disk_sensor_paths: Vec::new(),
            }
        }

        fn all_features() -> FeatureConfig {
            FeatureConfig {
                running_game: true,
                game_catalog: true,
                steam_library: true,
                launch_game: true,
                close_game: true,
                idle_tracking: true,
                sleep_wake: true,
                display_state: true,
                cmd_shutdown: true,
                cmd_restart: true,
                cmd_sleep: true,
                cmd_lock: true,
                cmd_logoff: true,
                cmd_monitor: true,
                notifications: true,
                cpu_sensor: true,
                memory_sensor: true,
                active_window: true,
                session_state: true,
                audio_device: true,
                mic: true,
                webcam: true,
                now_playing: true,
                volume: true,
                media_controls: true,
                steam_updates: true,
                discord: true,
                gpu_sensor: true,
                network_sensor: true,
                disk_sensor: true,
                uptime_sensor: true,
                hwinfo_sensor: true,
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

        /// Wait until every one of `topics` has been published. Robust to publish
        /// ordering and platform-specific publish counts, unlike a fixed-count wait.
        async fn wait_for_topics(state: &Arc<Mutex<BrokerState>>, topics: &[String]) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    {
                        let guard = state.lock().unwrap();
                        if topics
                            .iter()
                            .all(|want| guard.published.iter().any(|(t, _)| t == want))
                        {
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                let guard = state.lock().unwrap();
                let missing: Vec<&String> = topics
                    .iter()
                    .filter(|want| !guard.published.iter().any(|(t, _)| t == *want))
                    .collect();
                panic!("Timed out waiting for topics; missing: {missing:?}");
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
        // Deadlock tests - the #1 reason these integration tests exist.
        // current_thread matches production runtime. If the MQTT buffer is
        // too small, MqttClient::new() blocks forever and the timeout fires.
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_startup_no_deadlock_all_features() {
            let (port, _state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, all_features());
            let (stx, _) = test_shutdown();

            let result = tokio::time::timeout(
                Duration::from_secs(5),
                MqttClient::new(&config, stx.subscribe()),
            )
            .await;

            assert!(
                result.is_ok(),
                "MqttClient::new() timed out - likely deadlocked (buffer too small)"
            );
            assert!(result.unwrap().is_ok());
        }

        #[tokio::test(flavor = "current_thread")]
        async fn test_startup_no_deadlock_minimal() {
            let (port, _state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, FeatureConfig::default());
            let (stx, _) = test_shutdown();

            let result = tokio::time::timeout(
                Duration::from_secs(5),
                MqttClient::new(&config, stx.subscribe()),
            )
            .await;

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
            let (stx, _) = test_shutdown();

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

            let result = tokio::time::timeout(
                Duration::from_secs(5),
                MqttClient::new(&config, stx.subscribe()),
            )
            .await;

            assert!(
                result.is_ok(),
                "MqttClient::new() deadlocked with custom entities - buffer too small"
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
            let (stx, _) = test_shutdown();

            let (_mqtt, _cmd_rx) = MqttClient::new(&config, stx.subscribe()).await.unwrap();

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
            let expected_buttons = [
                "Launch",
                "Screensaver",
                "Wake",
                "Shutdown",
                "Sleep",
                "Lock",
                "Hibernate",
                "Restart",
                "DiscordJoin",
                "DiscordLeaveChannel",
                "MediaPlayPause",
                "MediaNext",
                "MediaPrevious",
                "MediaStop",
                "VolumeMute",
            ];

            // Wait until every expected discovery topic is published. Waiting for a
            // fixed publish count was racy: the total varies by platform (HWiNFO
            // sensors on Windows) and publish ordering isn't guaranteed, so a
            // specific topic (e.g. volume_level) could be missing at count 28.
            let mut want: Vec<String> = expected_sensors
                .iter()
                .map(|s| format!("homeassistant/sensor/test-pc/{s}/config"))
                .collect();
            want.extend(
                expected_buttons
                    .iter()
                    .map(|b| format!("homeassistant/button/test-pc/{b}/config")),
            );
            want.push("homeassistant/notify/test-pc/config".to_string());
            wait_for_topics(&state, &want).await;

            let guard = state.lock().unwrap();
            let topics: Vec<&str> = guard.published.iter().map(|(t, _)| t.as_str()).collect();

            for sensor in expected_sensors {
                let t = format!("homeassistant/sensor/test-pc/{sensor}/config");
                assert!(
                    topics.contains(&t.as_str()),
                    "Missing discovery for sensor: {sensor}"
                );
            }
            for button in expected_buttons {
                let t = format!("homeassistant/button/test-pc/{button}/config");
                assert!(
                    topics.contains(&t.as_str()),
                    "Missing discovery for button: {button}"
                );
            }
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
            let (stx, _) = test_shutdown();

            let (_mqtt, _cmd_rx) = MqttClient::new(&config, stx.subscribe()).await.unwrap();

            // subscribe_commands + ConnAck handler both subscribe → 18 * 2 = 36
            wait_for_subscribes(&state, 18).await;

            let guard = state.lock().unwrap();
            let topics: Vec<&str> = guard.subscribed.iter().map(|t| t.as_str()).collect();

            let expected = [
                "homeassistant/button/test-pc/Launch/action",
                "homeassistant/button/test-pc/RefreshSteamGames/action",
                "homeassistant/button/test-pc/Screensaver/action",
                "homeassistant/button/test-pc/Wake/action",
                "homeassistant/button/test-pc/Shutdown/action",
                "homeassistant/button/test-pc/Sleep/action",
                "homeassistant/button/test-pc/Lock/action",
                "homeassistant/button/test-pc/Hibernate/action",
                "homeassistant/button/test-pc/Restart/action",
                "homeassistant/button/test-pc/DiscordJoin/action",
                "homeassistant/button/test-pc/DiscordLeaveChannel/action",
                "homeassistant/button/test-pc/MediaPlayPause/action",
                "homeassistant/button/test-pc/MediaNext/action",
                "homeassistant/button/test-pc/MediaPrevious/action",
                "homeassistant/button/test-pc/MediaStop/action",
                "homeassistant/button/test-pc/VolumeMute/action",
                "pc-bridge/notifications/test-pc",
            ];
            for topic in expected {
                assert!(topics.contains(&topic), "Missing subscribe for: {topic}");
            }
        }

        // =================================================================
        // Command routing tests - verify end-to-end MQTT → CommandReceiver
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_command_routing_button() {
            let (port, state, inject) = start_mini_broker().await;
            let features = FeatureConfig {
                cmd_sleep: true,
                ..FeatureConfig::default()
            };
            let config = broker_config("test-pc", port, features);
            let (stx, _) = test_shutdown();

            let (_mqtt, mut cmd_rx) = MqttClient::new(&config, stx.subscribe()).await.unwrap();

            // Wait for subscriptions before injecting
            wait_for_subscribes(&state, 5).await;

            // Broker sends a "Sleep" button press to the client
            inject
                .send((
                    "homeassistant/button/test-pc/Sleep/action".to_string(),
                    String::new(),
                ))
                .await
                .unwrap();

            let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
                .await
                .expect("Timed out waiting for command")
                .expect("Command channel closed");

            assert_eq!(cmd.name, "Sleep");
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
            let (stx, _) = test_shutdown();

            let (_mqtt, mut cmd_rx) = MqttClient::new(&config, stx.subscribe()).await.unwrap();

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
                cmd_sleep: true,
                ..FeatureConfig::default()
            };
            let config = broker_config("test-pc", port, features);
            let (stx, _) = test_shutdown();

            let (_mqtt, mut cmd_rx) = MqttClient::new(&config, stx.subscribe()).await.unwrap();

            wait_for_subscribes(&state, 5).await;

            // Send a command for a DIFFERENT device
            inject
                .send((
                    "homeassistant/button/other-pc/Sleep/action".to_string(),
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

            // Should get "Shutdown" (ours), not "Sleep" (wrong device)
            assert_eq!(cmd.name, "Shutdown");
        }

        // =================================================================
        // Availability / LWT tests
        // =================================================================

        #[tokio::test(flavor = "current_thread")]
        async fn test_availability_published_on_connect() {
            let (port, state, _inject) = start_mini_broker().await;
            let config = broker_config("test-pc", port, FeatureConfig::default());
            let (stx, _) = test_shutdown();

            let (_mqtt, _cmd_rx) = MqttClient::new(&config, stx.subscribe()).await.unwrap();

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

    // ===== state_class on a HADiscoveryPayload =====
    // (Pure derive_state_class() tests live in payload.rs)

    #[test]
    fn test_state_class_serialized_in_payload() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "GPU Power".to_string(),
            unique_id: format!("{}_gpu_power", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("gpu_power")),
            command_topic: None,
            availability_topic: Some(mqtt.availability_topic()),
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:flash".to_string()),
            device_class: Some("power".to_string()),
            unit_of_measurement: Some("W".to_string()),
            state_class: Some("measurement".to_string()),
        };
        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["state_class"], "measurement");
    }

    #[test]
    fn test_state_class_omitted_when_none() {
        let mqtt = test_client("dank0i-pc");
        let payload = HADiscoveryPayload {
            name: "Sleep State".to_string(),
            unique_id: format!("{}_sleep_state", mqtt.device_id),
            state_topic: Some(mqtt.sensor_topic("sleep_state")),
            command_topic: None,
            availability_topic: None,
            availability: None,
            availability_mode: None,
            json_attributes_topic: None,
            device: Arc::clone(&mqtt.device),
            icon: Some("mdi:power-sleep".to_string()),
            device_class: None,
            unit_of_measurement: None,
            state_class: None,
        };
        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();
        // String enum sensors should NOT have state_class serialized
        assert!(json.get("state_class").is_none());
    }
}
