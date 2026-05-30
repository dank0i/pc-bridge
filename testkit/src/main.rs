//! pc-bridge integration test kit.
//!
//! Drives the REAL pc-bridge binary, end to end, the way Home Assistant would.
//! Two modes:
//!
//! **dry-run** (default): spawns `pc-bridge --dry-run`, publishes every command
//! on its real MQTT topic, and asserts the canonical action the binary reports
//! to `pc-bridge/test/executed/<device>`. Destructive commands (Sleep/Shutdown)
//! are exercised safely because dry-run performs no OS side effect. Validates
//! routing for the whole command surface. Runs anywhere.
//!
//! **live** (`--live`): spawns `pc-bridge` normally (no dry-run), publishes a
//! real `Launch`, and verifies the binary ACTUALLY spawns the process - checked
//! against the OS with `pgrep`, so it works on macOS where the bridge's own
//! `/proc`-based detection does not. This exercises the real launch path
//! (MQTT → executor → real spawn) that dry-run skips. On a Linux/Windows host
//! the same launch additionally drives the bridge's `runninggames` detection;
//! the Steam-specific cold-boot path can only be exercised on the real PC.
//!
//! Requires an MQTT broker at `$TESTKIT_BROKER` (default `127.0.0.1:1883`).
//! Exits non-zero on any failure, so it can gate CI.
//!
//! Env overrides:
//! - `TESTKIT_BROKER`  host:port of the MQTT broker (default `127.0.0.1:1883`)
//! - `TESTKIT_DEVICE`  device name used in topics (default `testkit-pc`)
//! - `PC_BRIDGE_BIN`   path to the pc-bridge binary (default: search ../target)

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tokio::time::timeout;

const DEFAULT_BROKER: &str = "127.0.0.1:1883";
const DEFAULT_DEVICE: &str = "testkit-pc";
/// How long to wait for the binary to come online before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a single command's `test/executed` record.
const CASE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for a real launched process to appear (live mode).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One dry-run command to drive and the canonical action it should resolve to.
struct Case {
    label: String,
    command: String,
    topic: String,
    payload: String,
    expect: String,
}

fn button_topic(device: &str, cmd: &str) -> String {
    format!("homeassistant/button/{device}/{cmd}/action")
}

/// The full dry-run command matrix: power, games + every launcher shortcut,
/// media, volume, notification, Discord, and a custom command.
fn cases(device: &str) -> Vec<Case> {
    let button = |cmd: &str, payload: &str, expect: &str, label: &str| Case {
        label: label.to_string(),
        command: cmd.to_string(),
        topic: button_topic(device, cmd),
        payload: payload.to_string(),
        expect: expect.to_string(),
    };

    vec![
        button("Wake", "PRESS", "native:wake", "power: wake"),
        button("Lock", "PRESS", "native:lock", "power: lock"),
        button("Shutdown", "PRESS", "native:shutdown", "power: shutdown"),
        button("Sleep", "PRESS", "native:sleep", "power: sleep"),
        button("Hibernate", "PRESS", "native:hibernate", "power: hibernate"),
        button("Restart", "PRESS", "native:restart", "power: restart"),
        button(
            "RefreshSteamGames",
            "PRESS",
            "native:refresh_steam_games",
            "steam: refresh",
        ),
        button("Screensaver", "PRESS", "native:screensaver", "screensaver"),
        // Game launches - one per launcher shortcut kind. The dry-run action is
        // the canonical launch intent; the platform-specific shell expansion is
        // covered by the launcher unit tests.
        button(
            "Launch",
            "steam:230410",
            "launch:steam:230410",
            "launch: steam",
        ),
        button(
            "Launch",
            "update:730",
            "launch:update:730",
            "launch: steam update",
        ),
        button(
            "Launch",
            "epic:Fortnite",
            "launch:epic:Fortnite",
            "launch: epic",
        ),
        button(
            "Launch",
            r"exe:C:\Games\Game.exe",
            r"launch:exe:C:\Games\Game.exe",
            "launch: exe",
        ),
        button(
            "Launch",
            r"lnk:C:\Games\Game.lnk",
            r"launch:lnk:C:\Games\Game.lnk",
            "launch: lnk",
        ),
        button(
            "Launch",
            "url:discord://channels/1/2",
            "launch:url:discord://channels/1/2",
            "launch: url",
        ),
        button(
            "Launch",
            "close:notepad",
            "launch:close:notepad",
            "launch: close process",
        ),
        button(
            "MediaPlayPause",
            "PRESS",
            "media:play_pause",
            "media: play/pause",
        ),
        button("MediaNext", "PRESS", "media:next", "media: next"),
        button(
            "MediaPrevious",
            "PRESS",
            "media:previous",
            "media: previous",
        ),
        button("MediaStop", "PRESS", "media:stop", "media: stop"),
        button("VolumeSet", "30", "volume:set:30", "volume: set 30"),
        button(
            "VolumeMute",
            "PRESS",
            "volume:mute_toggle",
            "volume: mute toggle",
        ),
        button("VolumeMute", "true", "volume:mute:true", "volume: mute on"),
        button(
            "DiscordLeaveChannel",
            "PRESS",
            "keybind:ctrl+f6",
            "discord: leave channel",
        ),
        button("TestCustom", "PRESS", "custom:TestCustom", "custom command"),
        Case {
            label: "notification".to_string(),
            command: "notification".to_string(),
            topic: format!("pc-bridge/notifications/{device}"),
            payload: "Build complete".to_string(),
            expect: "notification:Build complete".to_string(),
        },
    ]
}

/// Config for the dry-run suite.
fn dryrun_config(device: &str, broker_url: &str) -> String {
    let cfg = serde_json::json!({
        "device_name": device,
        "mqtt": { "broker": broker_url, "user": "", "pass": "" },
        // Core commands (power, launch, screensaver, discord) are always
        // subscribed; enable the rest needed to subscribe the commands tested.
        "features": {
            "power_events": false,
            "game_detection": false,
            "audio_control": true,
            "notifications": true,
            "system_sensors": false
        },
        "custom_commands_enabled": true,
        "allow_raw_commands": false,
        "games": {},
        "custom_commands": [
            { "name": "TestCustom", "type": "shell", "command": "echo testkit" }
        ]
    });
    serde_json::to_string_pretty(&cfg).expect("config serializes")
}

/// Config for the live launch suite. Allows raw commands so we can launch a
/// controllable marker process and verify the binary really spawns it.
fn live_config(device: &str, broker_url: &str) -> String {
    let cfg = serde_json::json!({
        "device_name": device,
        "mqtt": { "broker": broker_url, "user": "", "pass": "" },
        "features": {
            "power_events": false,
            "game_detection": false,
            "audio_control": false,
            "notifications": false,
            "system_sensors": false
        },
        "custom_commands_enabled": false,
        "allow_raw_commands": true,
        "games": {},
        "custom_commands": []
    });
    serde_json::to_string_pretty(&cfg).expect("config serializes")
}

/// Locate the pc-bridge binary: `$PC_BRIDGE_BIN`, else common build outputs.
fn find_pc_bridge_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("PC_BRIDGE_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "PC_BRIDGE_BIN is set but does not exist: {}",
            path.display()
        );
    }
    let candidates = [
        "../target/release/pc-bridge",
        "../target/debug/pc-bridge",
        "../target/release/pc-bridge.exe",
        "../target/debug/pc-bridge.exe",
    ];
    for c in candidates {
        let path = PathBuf::from(c);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "could not find the pc-bridge binary. Build it first (cargo build --release \
         in the pc-bridge dir) or set PC_BRIDGE_BIN to its path."
    )
}

/// Messages forwarded from the MQTT event loop to the test driver.
enum Incoming {
    /// Availability flipped (true = online).
    Availability(bool),
    /// A dry-run command record arrived on the test topic.
    Record(serde_json::Value),
}

/// Connect an MQTT client, subscribe to availability + test topics, and spawn a
/// task that forwards relevant publishes. Returns the client, a receiver, and
/// the pump task handle.
async fn connect(
    host: &str,
    port: u16,
    device: &str,
) -> Result<(
    AsyncClient,
    mpsc::UnboundedReceiver<Incoming>,
    tokio::task::JoinHandle<()>,
)> {
    let mut opts = MqttOptions::new(format!("testkit-{}", std::process::id()), host, port);
    opts.set_keep_alive(Duration::from_secs(30));
    let (client, mut eventloop) = AsyncClient::new(opts, 128);

    let avail_topic = format!("homeassistant/sensor/{device}/availability");
    let test_topic = format!("pc-bridge/test/executed/{device}");
    client.subscribe(&avail_topic, QoS::AtLeastOnce).await?;
    client.subscribe(&test_topic, QoS::AtLeastOnce).await?;

    let (tx, rx) = mpsc::unbounded_channel::<Incoming>();
    let pump = tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    if p.topic == avail_topic {
                        let _ = tx.send(Incoming::Availability(p.payload.as_ref() == b"online"));
                    } else if p.topic == test_topic
                        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&p.payload)
                    {
                        let _ = tx.send(Incoming::Record(v));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("mqtt event loop error: {e}");
                    break;
                }
            }
        }
    });
    Ok((client, rx, pump))
}

/// Spawn the pc-bridge binary against a config dir, inheriting stderr.
fn spawn_bridge(bin: &Path, cfg_dir: &Path, dry_run: bool) -> Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(bin);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.env("PC_BRIDGE_CONFIG_DIR", cfg_dir)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn pc-bridge")
}

/// Wait for the binary to announce `availability = online`.
async fn wait_online(rx: &mut mpsc::UnboundedReceiver<Incoming>) -> Result<()> {
    let online = timeout(READY_TIMEOUT, async {
        while let Some(msg) = rx.recv().await {
            if let Incoming::Availability(true) = msg {
                return true;
            }
        }
        false
    })
    .await;
    match online {
        Ok(true) => Ok(()),
        _ => bail!(
            "pc-bridge did not come online within {}s (broker unreachable, or the binary failed \
             to start - check its stderr above)",
            READY_TIMEOUT.as_secs()
        ),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let live = std::env::args().any(|a| a == "--live")
        || matches!(std::env::var("TESTKIT_MODE").as_deref(), Ok("live"));

    let broker = std::env::var("TESTKIT_BROKER").unwrap_or_else(|_| DEFAULT_BROKER.to_string());
    let device = std::env::var("TESTKIT_DEVICE").unwrap_or_else(|_| DEFAULT_DEVICE.to_string());
    let (host, port) = parse_host_port(&broker)?;
    let broker_url = format!("tcp://{host}:{port}");

    let bin = find_pc_bridge_binary()?;
    println!("pc-bridge binary : {}", bin.display());
    println!("broker           : {host}:{port}");
    println!("device           : {device}");
    println!(
        "mode             : {}\n",
        if live { "live" } else { "dry-run" }
    );

    if live {
        run_live(&bin, &host, port, &device, &broker_url).await
    } else {
        run_dryrun(&bin, &host, port, &device, &broker_url).await
    }
}

/// Dry-run suite: drive every command, assert the reported canonical action.
async fn run_dryrun(
    bin: &Path,
    host: &str,
    port: u16,
    device: &str,
    broker_url: &str,
) -> Result<()> {
    let cfg_dir = temp_config_dir();
    std::fs::create_dir_all(&cfg_dir).context("create temp config dir")?;
    std::fs::write(
        cfg_dir.join("userConfig.json"),
        dryrun_config(device, broker_url),
    )
    .context("write test config")?;

    let (client, mut rx, pump) = connect(host, port, device).await?;
    let mut child = spawn_bridge(bin, &cfg_dir, true)?;

    let result = async {
        wait_online(&mut rx).await?;
        println!(
            "pc-bridge is online - running {} cases\n",
            cases(device).len()
        );
        let mut report: Report = Vec::new();
        for case in cases(device) {
            while rx.try_recv().is_ok() {} // drop stale records
            client
                .publish(
                    &case.topic,
                    QoS::AtLeastOnce,
                    false,
                    case.payload.as_bytes(),
                )
                .await
                .with_context(|| format!("publish {}", case.label))?;
            let (ok, detail) = match timeout(CASE_TIMEOUT, next_record(&mut rx)).await {
                Ok(Some(rec)) => check(&case, &rec),
                Ok(None) => (
                    false,
                    "event loop closed before a record arrived".to_string(),
                ),
                Err(_) => (
                    false,
                    format!("timed out after {}s with no record", CASE_TIMEOUT.as_secs()),
                ),
            };
            println!("  {} {}", if ok { "PASS" } else { "FAIL" }, case.label);
            report.push((case.label, ok, detail));
        }
        Ok::<Report, anyhow::Error>(report)
    }
    .await;

    let _ = child.kill().await;
    pump.abort();
    let _ = std::fs::remove_dir_all(&cfg_dir);

    let report = result?;
    print_report(&report);
    if report.iter().all(|(_, ok, _)| *ok) {
        println!("\nALL {} CASES PASSED", report.len());
        Ok(())
    } else {
        let failed = report.iter().filter(|(_, ok, _)| !*ok).count();
        bail!("{failed} of {} cases FAILED", report.len());
    }
}

/// Live suite: publish a real `Launch` and verify the binary actually spawns the
/// process (checked against the OS, so it works where `/proc` detection cannot).
async fn run_live(bin: &Path, host: &str, port: u16, device: &str, broker_url: &str) -> Result<()> {
    // A uniquely-named, long-lived marker the bridge will spawn via `bash -c`.
    // `exec -a <marker>` makes the marker the process's argv[0], so it survives
    // bash's exec optimization (a plain `sleep N # marker` would get rewritten
    // to bare `sleep N`, dropping the marker). `pgrep -f` then finds exactly it,
    // and `pkill -f` cleans it up with no orphaned child. The 120s cap is a
    // safety net in case cleanup is ever missed.
    let marker = format!("pcbridge-testkit-marker-{}", std::process::id());
    let launch_cmd = format!("exec -a {marker} sleep 120");

    if process_running(&marker).await {
        bail!("marker process already running before the test - aborting");
    }

    let cfg_dir = temp_config_dir();
    std::fs::create_dir_all(&cfg_dir).context("create temp config dir")?;
    std::fs::write(
        cfg_dir.join("userConfig.json"),
        live_config(device, broker_url),
    )
    .context("write live config")?;

    let (client, mut rx, pump) = connect(host, port, device).await?;
    let mut child = spawn_bridge(bin, &cfg_dir, false)?;

    let result = async {
        wait_online(&mut rx).await?;
        println!("pc-bridge is online - running live launch test\n");

        // Publish a real Launch (no dry-run): the bridge should execute it.
        client
            .publish(
                button_topic(device, "Launch"),
                QoS::AtLeastOnce,
                false,
                launch_cmd.as_bytes(),
            )
            .await
            .context("publish Launch")?;

        // Verify the bridge actually spawned the process.
        let appeared = wait_for_process(&marker, LAUNCH_TIMEOUT).await;
        Ok::<bool, anyhow::Error>(appeared)
    }
    .await;

    // Tear down: stop the bridge, kill the (possibly orphaned) marker, clean up.
    let _ = child.kill().await;
    pump.abort();
    kill_process(&marker).await;
    let _ = std::fs::remove_dir_all(&cfg_dir);

    let spawned = result?;
    println!("\n================ RESULTS ================");
    if spawned {
        println!("  PASS  live launch: bridge spawned the launched process");
        println!("========================================");
        println!("\nLIVE LAUNCH TEST PASSED");
        Ok(())
    } else {
        println!(
            "  FAIL  live launch: process did not appear within {}s",
            LAUNCH_TIMEOUT.as_secs()
        );
        println!("========================================");
        bail!("live launch test FAILED - the bridge did not spawn the process");
    }
}

/// (label, passed, detail) per dry-run case.
type Report = Vec<(String, bool, String)>;

async fn next_record(rx: &mut mpsc::UnboundedReceiver<Incoming>) -> Option<serde_json::Value> {
    while let Some(msg) = rx.recv().await {
        if let Incoming::Record(v) = msg {
            return Some(v);
        }
    }
    None
}

fn check(case: &Case, rec: &serde_json::Value) -> (bool, String) {
    let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let action = rec.get("action").and_then(|v| v.as_str()).unwrap_or("");
    if name != case.command {
        return (
            false,
            format!("routed to '{name}', expected '{}'", case.command),
        );
    }
    if action != case.expect {
        return (
            false,
            format!("action '{action}', expected '{}'", case.expect),
        );
    }
    (true, action.to_string())
}

fn print_report(report: &Report) {
    println!("\n================ RESULTS ================");
    for (label, ok, detail) in report {
        if *ok {
            println!("  PASS  {label}  ->  {detail}");
        } else {
            println!("  FAIL  {label}  ::  {detail}");
        }
    }
    println!("========================================");
}

/// Is a process whose command line matches `marker` currently running?
async fn process_running(marker: &str) -> bool {
    tokio::process::Command::new("pgrep")
        .arg("-f")
        .arg(marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll until a matching process appears or the timeout elapses.
async fn wait_for_process(marker: &str, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if process_running(marker).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Best-effort kill of the marker process (cleanup).
async fn kill_process(marker: &str) {
    let _ = tokio::process::Command::new("pkill")
        .arg("-f")
        .arg(marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Throwaway config dir, namespaced by PID so parallel runs don't collide.
fn temp_config_dir() -> PathBuf {
    std::env::temp_dir().join(format!("pc-bridge-testkit-{}", std::process::id()))
}

/// Split `host:port`, defaulting the port to 1883 when omitted.
fn parse_host_port(s: &str) -> Result<(String, u16)> {
    match s.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .parse::<u16>()
                .with_context(|| format!("invalid port in '{s}'"))?;
            Ok((h.to_string(), port))
        }
        None => Ok((s.to_string(), 1883)),
    }
}
