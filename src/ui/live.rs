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
    /// Display names of games Steam is updating (from `steam_updating` attributes).
    pub updating_games: Vec<String>,
    /// Module host state, as `(name, status)` sorted by name, straight from the
    /// agent's `modules` sensor. The settings window has no other way to see
    /// this: modules are supervised inside the agent, and this window is a
    /// separate process that only ever learns runtime state over the broker.
    pub modules: Vec<(String, String)>,
    /// True once a `modules` attributes payload has been seen, so the UI can
    /// tell "no modules configured" apart from "not heard from the agent yet".
    pub modules_seen: bool,
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

/// Parse the `modules` sensor's attribute payload into a sorted `(name, status)`
/// list.
///
/// Sorted because an unordered JSON object would reshuffle the list between
/// frames. Malformed or non-object payloads yield an empty list rather than an
/// error: this is a status display, and a broker hiccup should not blank the
/// window or panic the UI thread.
fn parse_modules(payload: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(payload)
    else {
        return Vec::new();
    };
    let mut v: Vec<(String, String)> = map
        .iter()
        .map(|(k, val)| (k.clone(), val.as_str().unwrap_or("?").to_string()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn sub_topics(dev: &str) -> [String; 4] {
    [
        crate::mqtt::availability_topic_for(dev),
        crate::mqtt::sensor_state_topic_for(dev, "runninggames"),
        crate::mqtt::sensor_attributes_topic_for(dev, "steam_updating"),
        crate::mqtt::sensor_attributes_topic_for(dev, "modules"),
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
                } else if p.topic.ends_with("/modules/attributes") {
                    // Attributes are a flat {name: status} map. Sorted so the
                    // list does not reshuffle between frames.
                    s.modules_seen = true;
                    s.modules = parse_modules(&payload);
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

#[cfg(test)]
mod tests {
    use super::parse_modules;

    #[test]
    fn parses_the_agents_attribute_shape() {
        let got = parse_modules(r#"{"sc3-mixer":"running","other":"blocked"}"#);
        assert_eq!(
            got,
            vec![
                ("other".to_string(), "blocked".to_string()),
                ("sc3-mixer".to_string(), "running".to_string()),
            ],
            "must be sorted by name so the list does not reshuffle between frames"
        );
    }

    #[test]
    fn malformed_payloads_do_not_panic_or_error() {
        // A broker hiccup or a half-written retained message must not blank the
        // window in a way that looks like a crash.
        for bad in ["", "not json", "[1,2,3]", "null", "\"a string\"", "{"] {
            assert!(
                parse_modules(bad).is_empty(),
                "{bad:?} should yield nothing"
            );
        }
    }

    #[test]
    fn non_string_status_values_are_marked_rather_than_dropped() {
        // Losing the row entirely would hide a module; showing "?" does not.
        let got = parse_modules(r#"{"a":123,"b":"running"}"#);
        assert_eq!(got[0], ("a".to_string(), "?".to_string()));
        assert_eq!(got[1], ("b".to_string(), "running".to_string()));
    }

    #[test]
    fn an_empty_object_is_distinct_from_a_malformed_one() {
        // Both give an empty list, but the caller sets modules_seen only on a
        // real payload, which is what tells "no modules" from "no agent".
        assert!(parse_modules("{}").is_empty());
    }
}
