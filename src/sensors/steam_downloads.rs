//! Live Steam download progress via Steam's CEF (Chromium) debugger.
//!
//! Steam's UI is Chromium Embedded Framework. With an empty
//! `.cef-enable-remote-debugging` file in the Steam folder plus a Steam restart,
//! Steam exposes the CEF DevTools protocol on 127.0.0.1:8080. We attach to the
//! SharedJSContext target, register `SteamClient.Downloads.RegisterForDownloadOverview`
//! (it persists in Steam's JS context across our reconnects), and read the latest
//! overview each tick to publish live download %, speed, and the app id.
//!
//! The ACF-based `steam_updating` sensor is on/off only; this adds the percentage.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use serde_json::json;
use tokio::time::{MissedTickBehavior, interval};
use tungstenite::Message;

use crate::AppState;

const CEF_HOST: &str = "127.0.0.1";
const CEF_PORT: u16 = 8080;

/// Registers the download-overview callback once, then returns the latest value.
/// The registration lives in Steam's own JS context, so it survives our websocket
/// reconnecting each poll.
const EVAL_JS: &str = r#"(function(){try{if(!window.__pcb){window.__pcb={ov:null};if(typeof SteamClient!=='undefined'&&SteamClient.Downloads&&SteamClient.Downloads.RegisterForDownloadOverview){SteamClient.Downloads.RegisterForDownloadOverview(function(o){window.__pcb.ov=o;});}}return JSON.stringify(window.__pcb);}catch(e){return JSON.stringify({error:String(e)});}})()"#;

pub struct SteamDownloadsSensor {
    state: Arc<AppState>,
}

impl SteamDownloadsSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut tick = interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Best effort: drop the enable-file so a Steam restart turns debugging on.
        let _ = tokio::task::spawn_blocking(ensure_cef_enabled).await;

        info!("Steam download sensor started (CEF debugger @ {CEF_HOST}:{CEF_PORT})");
        let mut prev = String::new();
        let mut warned = false;

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Steam download sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev.clear();
                }
                _ = tick.tick() => {
                    // The CEF handshake + read blocks; keep it off the runtime.
                    let outcome = tokio::task::spawn_blocking(poll_download).await;
                    let (state_str, attrs) = match outcome {
                        Ok(Ok(Some(d))) => {
                            warned = false;
                            (format!("{:.0}", d.percent), d.attributes())
                        }
                        Ok(Ok(None)) => {
                            // Steam up, nothing downloading -> 0% (keeps it numeric).
                            warned = false;
                            ("0".to_string(), json!({ "state": "idle" }))
                        }
                        Ok(Err(e)) => {
                            // Steam not running, or CEF debugging not enabled yet.
                            if !warned {
                                warn!(
                                    "Steam download progress unavailable ({e}). To enable it: \
                                     add an empty file named '.cef-enable-remote-debugging' to your \
                                     Steam folder and restart Steam."
                                );
                                warned = true;
                            }
                            ("unavailable".to_string(), json!({ "state": "unavailable" }))
                        }
                        Err(_) => continue, // spawn_blocking join error
                    };

                    if state_str != prev {
                        self.state
                            .mqtt
                            .publish_sensor_retained("steam_download", &state_str)
                            .await;
                        self.state
                            .mqtt
                            .publish_sensor_attributes("steam_download", &attrs)
                            .await;
                        prev = state_str;
                    }
                }
            }
        }
    }
}

struct Download {
    percent: f64,
    appid: u64,
    bytes_per_sec: i64,
    overview: serde_json::Value,
}

impl Download {
    fn attributes(&self) -> serde_json::Value {
        json!({
            "state": "downloading",
            "app_id": self.appid,
            "percent": self.percent,
            "bytes_per_second": self.bytes_per_sec,
            // bytes/s -> Mbit/s (÷ 125000)
            "mbps": (self.bytes_per_sec as f64 / 125_000.0 * 10.0).round() / 10.0,
            // Raw overview for debugging / any field HA wants to template on.
            "overview": self.overview,
        })
    }
}

/// Create the `.cef-enable-remote-debugging` marker in the Steam folder so a Steam
/// restart turns the debugger on. Best effort (may need admin on Program Files).
fn ensure_cef_enabled() {
    let Some(steam) = crate::steam::find_steam_path() else {
        return;
    };
    let flag = steam.join(".cef-enable-remote-debugging");
    if flag.exists() {
        return;
    }
    match std::fs::File::create(&flag) {
        Ok(_) => info!(
            "Enabled Steam CEF debugging ({}); restart Steam once for live download progress.",
            flag.display()
        ),
        Err(e) => warn!(
            "Couldn't create {} ({e}); create it manually (empty file) and restart Steam.",
            flag.display()
        ),
    }
}

/// One poll: find the SharedJSContext debugger, evaluate the overview JS, parse.
/// Returns `Ok(None)` when Steam is up but nothing is actively downloading.
fn poll_download() -> anyhow::Result<Option<Download>> {
    // 1) List debugger targets, find SharedJSContext's websocket URL.
    let list_url = format!("http://{CEF_HOST}:{CEF_PORT}/json");
    let body = ureq::get(&list_url).call()?.body_mut().read_to_string()?;
    let targets: Vec<serde_json::Value> = serde_json::from_str(&body)?;
    let ws_url = targets
        .iter()
        .find(|t| {
            let has = |k: &str| {
                t.get(k)
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.contains("SharedJSContext"))
            };
            has("title") || has("url")
        })
        .and_then(|t| t.get("webSocketDebuggerUrl"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("SharedJSContext debugger not found"))?
        .to_string();

    // 2) Connect the websocket over plain loopback TCP.
    let stream = TcpStream::connect((CEF_HOST, CEF_PORT))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (mut ws, _resp) = tungstenite::client::client(&ws_url, stream)?;

    // 3) Runtime.evaluate the overview JS.
    let cmd = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": { "expression": EVAL_JS, "returnByValue": true }
    });
    ws.send(Message::Text(cmd.to_string()))?;

    let reply = loop {
        match ws.read()? {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(t.as_str())?;
                if v.get("id").and_then(serde_json::Value::as_i64) == Some(1) {
                    break v;
                }
            }
            Message::Close(_) => anyhow::bail!("websocket closed"),
            _ => {}
        }
    };
    let _ = ws.close(None);

    let json_str = reply["result"]["result"]["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no evaluate result"))?;
    let data: serde_json::Value = serde_json::from_str(json_str)?;
    let ov = &data["ov"];
    if ov.is_null() {
        // Callback hasn't fired yet, or no overview available.
        return Ok(None);
    }

    let to_dl = ov["update_bytes_to_download"].as_f64().unwrap_or(0.0);
    let done = ov["update_bytes_downloaded"].as_f64().unwrap_or(0.0);
    let state = ov["update_state"].as_str().unwrap_or("");
    let active = to_dl > 0.0 && !matches!(state, "None" | "" | "Stopping");
    if !active {
        return Ok(None);
    }

    let percent = (done / to_dl * 100.0).clamp(0.0, 100.0);
    Ok(Some(Download {
        percent,
        appid: ov["update_appid"].as_u64().unwrap_or(0),
        bytes_per_sec: ov["update_network_bytes_per_second"].as_i64().unwrap_or(0),
        overview: ov.clone(),
    }))
}
