//! Live view for the settings window: subscribes to the broker and reads the agent's
//! already-published sensors, so the games list and connection indicator reflect
//! REAL state instead of a config guess. Reuses the agent's detection (running game,
//! download %) rather than re-implementing process enumeration, and is fully
//! cross-platform.
//!
//! Runs a sync `rumqttc` client on a background thread and shares the latest values
//! through an `Arc<Mutex<LiveState>>` the egui loop reads each frame.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{Client, Event, MqttOptions, Packet, QoS};

use crate::config::Config;

#[derive(Default, Clone)]
pub struct LiveState {
    /// True once we've had any connection outcome (ConnAck or an error), so the UI
    /// can show "connecting..." before the first result instead of a false negative.
    pub attempted: bool,
    /// True once the MQTT connection is established (ConnAck seen).
    pub broker_connected: bool,
    /// Agent availability (retained `online`/`offline`); None until first received.
    pub agent_online: Option<bool>,
    /// Currently-running game id (agent's `runninggames` sensor), if any.
    pub running_game_id: Option<String>,
    /// App id of the game Steam is downloading, from `steam_download` attributes.
    pub download_appid: Option<u32>,
    /// Live download percent (0-100), from `steam_download` state.
    pub download_pct: Option<u8>,
    /// Display names of games Steam is updating (from `steam_updating` attributes).
    pub updating_games: Vec<String>,
}

pub struct LiveView {
    pub state: Arc<Mutex<LiveState>>,
}

impl LiveView {
    pub fn snapshot(&self) -> LiveState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Start the background subscriber for the given config's broker. The thread lives
/// for the process (the settings window is short-lived); it auto-reconnects.
pub fn start(cfg: &Config) -> LiveView {
    let state = Arc::new(Mutex::new(LiveState::default()));
    let st = Arc::clone(&state);
    let dev = cfg.device_name.clone();
    let broker = cfg.mqtt.broker.clone();
    let user = cfg.mqtt.user.clone();
    let pass = cfg.mqtt.pass.clone();
    std::thread::spawn(move || run(broker, user, pass, dev, st));
    LiveView { state }
}

fn sub_topics(dev: &str) -> [String; 5] {
    [
        format!("homeassistant/sensor/{dev}/availability"),
        format!("homeassistant/sensor/{dev}/runninggames/state"),
        format!("homeassistant/sensor/{dev}/steam_download/state"),
        format!("homeassistant/sensor/{dev}/steam_download/attributes"),
        format!("homeassistant/sensor/{dev}/steam_updating/attributes"),
    ]
}

fn run(broker: String, user: String, pass: String, dev: String, state: Arc<Mutex<LiveState>>) {
    // No broker configured (first run / load error): nothing to connect to.
    if broker.trim().is_empty() {
        if let Ok(mut s) = state.lock() {
            s.attempted = true;
        }
        return;
    }
    let (host, port, tls) = crate::power::sync_mqtt::parse_broker_url(&broker);
    // Distinct client id so we don't clash with the agent's session on the broker.
    let mut opts = MqttOptions::new(format!("pc-bridge-ui-{}", std::process::id()), host, port);
    opts.set_keep_alive(Duration::from_secs(30));
    if !user.is_empty() {
        opts.set_credentials(user, pass);
    }
    if tls {
        opts.set_transport(rumqttc::Transport::tls_with_config(
            rumqttc::TlsConfiguration::Native,
        ));
    }

    let (client, mut connection) = Client::new(opts, 10);
    let subs = sub_topics(&dev);

    for notification in connection.iter() {
        match notification {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                if let Ok(mut s) = state.lock() {
                    s.attempted = true;
                    s.broker_connected = true;
                }
                // clean_session drops subscriptions on reconnect, so (re)subscribe here.
                for t in &subs {
                    let _ = client.subscribe(t.as_str(), QoS::AtMostOnce);
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let payload = String::from_utf8_lossy(&p.payload);
                let val = payload.trim();
                let Ok(mut s) = state.lock() else { continue };
                if p.topic.ends_with("/availability") {
                    s.agent_online = Some(val.eq_ignore_ascii_case("online"));
                } else if p.topic.ends_with("/runninggames/state") {
                    s.running_game_id = match val {
                        "" | "none" | "None" | "unavailable" | "unknown" => None,
                        v => Some(v.to_string()),
                    };
                } else if p.topic.ends_with("/steam_download/state") {
                    s.download_pct = val.parse::<u8>().ok();
                } else if p.topic.ends_with("/steam_download/attributes") {
                    s.download_appid = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v.get("app_id").and_then(serde_json::Value::as_u64))
                        .map(|n| n as u32);
                } else if p.topic.ends_with("/steam_updating/attributes") {
                    s.updating_games = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| {
                            v.get("updating_games")
                                .and_then(serde_json::Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                        })
                        .unwrap_or_default();
                }
            }
            Err(_) => {
                if let Ok(mut s) = state.lock() {
                    s.attempted = true;
                    s.broker_connected = false;
                }
                // rumqttc retries on the next iter(); back off so we don't spin.
                std::thread::sleep(Duration::from_secs(2));
            }
            _ => {}
        }
    }
}
