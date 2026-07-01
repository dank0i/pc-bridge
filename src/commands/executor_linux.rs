//! Command executor for Linux - feature-parity with Windows executor

use log::{debug, error, info, warn};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::custom::execute_custom_command;
use super::launcher_linux::expand_launcher_shortcut;
use crate::AppState;
use crate::audio::{self, MediaKey};
use crate::mqtt::CommandReceiver;
use crate::notification;
use crate::power::sync_mqtt::{SyncMqttConfig, parse_broker_url, sync_mqtt_publish_sleep};
use crate::power::{monitor_off, wake_display};
use crate::steam::SteamGameDiscovery;

const MAX_CONCURRENT_COMMANDS: usize = 5;
/// How long to wait for Steam to come up before launching anyway.
const STEAM_WAIT_TIMEOUT_SECS: u64 = 90;
/// Grace period after Steam appears, for it to finish initializing.
const STEAM_INIT_DELAY_SECS: u64 = 12;

/// Predefined shell commands for Linux
fn get_predefined_command(name: &str) -> Option<&'static str> {
    match name {
        "Screensaver" => Some("xdg-screensaver activate"),
        "Wake" | "Sleep" | "Hibernate" | "MonitorOff" | "MonitorOn" | "CloseGame" => None, // Handled natively
        "Shutdown" => Some("systemctl poweroff"),
        "Lock" => Some("loginctl lock-session"),
        "Restart" => Some("systemctl reboot"),
        "Logoff" => Some("loginctl terminate-session \"$XDG_SESSION_ID\""),
        _ => None,
    }
}

pub struct CommandExecutor {
    state: Arc<AppState>,
    command_rx: CommandReceiver,
    semaphore: Arc<Semaphore>,
}

impl CommandExecutor {
    pub fn new(state: Arc<AppState>, command_rx: CommandReceiver) -> Self {
        Self {
            state,
            command_rx,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS)),
        }
    }

    pub async fn run(mut self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Command executor shutting down");
                    break;
                }
                Some(cmd) = self.command_rx.recv() => {
                    // Rate limit with semaphore
                    let permit = match self.semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            warn!("Command rate limited, dropping: {}", cmd.name);
                            continue;
                        }
                    };

                    let state_clone = self.state.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = Self::execute_command(&cmd.name, &cmd.payload, &state_clone).await {
                            error!("Command error: {}", e);
                        }
                    });
                }
            }
        }
    }

    async fn execute_command(
        name: &str,
        payload: &str,
        state: &Arc<AppState>,
    ) -> anyhow::Result<()> {
        let payload = payload.trim();
        let payload = if payload.eq_ignore_ascii_case("PRESS") {
            ""
        } else {
            payload
        };

        info!("Executing command: {} (payload: {:?})", name, payload);

        // Dry-run: report what the command would do to the test topic, but
        // perform no OS side effect. Shares the canonical resolver with the
        // Windows executor so the test kit sees identical results on any host.
        // Inert unless --dry-run / PC_BRIDGE_DRY_RUN is set.
        if state.dry_run {
            crate::commands::dry_run::report(name, payload, state).await;
            return Ok(());
        }

        // Defense: a disabled feature's command can still arrive via a stale
        // broker subscription. Drop it rather than execute (e.g. Shutdown).
        if !crate::commands::command_feature_enabled(name, &state.config.read().await.features) {
            warn!("Ignoring '{}' - its feature is disabled", name);
            return Ok(());
        }

        // ── Native commands (no shell needed) ──────────────────────────
        match name {
            "DiscordLeaveChannel" => {
                let keybind = state
                    .config
                    .read()
                    .await
                    .discord_keybind
                    .clone()
                    .unwrap_or_else(|| "ctrl+f6".to_string());
                // xdotool .status() blocks; keep it off the runtime (matches the
                // Windows DiscordLeaveChannel path).
                tokio::task::spawn_blocking(move || send_keybind_linux(&keybind));
                return Ok(());
            }
            "Wake" => {
                // wake_display spawns and waits on xdotool/xset/dbus-send; keep
                // it off the single-threaded runtime.
                tokio::task::spawn_blocking(wake_display);
                return Ok(());
            }
            "Sleep" | "Hibernate" => {
                // Pre-publish sleep state via sync TCP before the NIC goes down,
                // matching the Windows behavior in power/events.rs.
                let cfg = {
                    let config = state.config.read().await;
                    let (host, port, use_tls) = parse_broker_url(&config.mqtt.broker);
                    SyncMqttConfig {
                        host,
                        port,
                        use_tls,
                        user: config.mqtt.user.clone(),
                        pass: config.mqtt.pass.clone(),
                        client_id: format!("{}-sleep", config.client_id()),
                        sleep_topic: format!(
                            "homeassistant/sensor/{}/sleep_state/state",
                            config.device_name
                        ),
                    }
                };
                // Off the runtime: a broker connect timeout would otherwise stall
                // the single-threaded runtime.
                match tokio::task::spawn_blocking(move || sync_mqtt_publish_sleep(&cfg)).await {
                    Ok(Ok(())) => info!("Sleep state pre-published via sync TCP"),
                    Ok(Err(e)) => warn!("Sync MQTT sleep pre-publish failed: {}", e),
                    Err(e) => warn!("Sync publish task join error: {}", e),
                }
                // Also publish via async client as fallback
                state
                    .mqtt
                    .publish_sensor_retained("sleep_state", "sleeping")
                    .await;
                let cmd = if name == "Sleep" {
                    "systemctl suspend"
                } else {
                    "systemctl hibernate"
                };
                // .status() reaps the process (no zombie) and normally returns
                // fast (systemd suspends asynchronously), but a polkit prompt or
                // hung systemd could block, so run it off the single-threaded
                // runtime.
                let _ = tokio::task::spawn_blocking(move || {
                    Command::new("bash").args(["-c", cmd]).status()
                })
                .await;
                return Ok(());
            }
            "MonitorOff" => {
                tokio::task::spawn_blocking(monitor_off);
                return Ok(());
            }
            "MonitorOn" => {
                tokio::task::spawn_blocking(wake_display);
                return Ok(());
            }
            "CloseGame" => {
                close_running_games(state).await;
                return Ok(());
            }
            "notification" => {
                if !payload.is_empty() {
                    // notify-send/gdbus .status() block; keep them off the runtime.
                    let p = payload.to_string();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = notification::show_toast(&p) {
                            warn!("Failed to show notification: {e}");
                        }
                    });
                }
                return Ok(());
            }
            "VolumeSet" => {
                if let Ok(level) = payload.parse::<f32>() {
                    tokio::task::spawn_blocking(move || audio::set_volume(level));
                }
                return Ok(());
            }
            "VolumeMute" => {
                if payload.eq_ignore_ascii_case("press") || payload.is_empty() {
                    tokio::task::spawn_blocking(audio::toggle_mute);
                } else {
                    let mute = payload.eq_ignore_ascii_case("true") || payload == "1";
                    tokio::task::spawn_blocking(move || audio::set_mute(mute));
                }
                return Ok(());
            }
            "MediaPlayPause" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::PlayPause));
                return Ok(());
            }
            "MediaNext" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Next));
                return Ok(());
            }
            "MediaPrevious" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Previous));
                return Ok(());
            }
            "MediaStop" => {
                tokio::task::spawn_blocking(|| audio::send_media_key(MediaKey::Stop));
                return Ok(());
            }
            "RefreshSteamGames" => {
                info!("Refreshing Steam game library...");
                match SteamGameDiscovery::discover_async().await {
                    Some(discovery) => {
                        // Merge in memory under the lock, then release it before
                        // the (blocking) disk save so we don't stall the runtime
                        // or block every other task's config read.
                        let snapshot = {
                            let mut config = state.config.write().await;
                            let (added, removed) = config.merge_steam_games(&discovery);
                            if added > 0 || removed > 0 {
                                info!(
                                    "Steam refresh: +{} added, -{} removed ({}ms{})",
                                    added,
                                    removed,
                                    discovery.build_time_ms,
                                    if discovery.from_cache { ", cached" } else { "" }
                                );
                                Some(config.clone())
                            } else {
                                info!(
                                    "Steam refresh: no changes ({}ms{})",
                                    discovery.build_time_ms,
                                    if discovery.from_cache { ", cached" } else { "" }
                                );
                                None
                            }
                        };
                        if let Some(cfg) = snapshot {
                            match tokio::task::spawn_blocking(move || cfg.save()).await {
                                Ok(Ok(())) => {
                                    let _ = state.config_generation.send(());
                                }
                                Ok(Err(e)) => warn!("Steam refresh: failed to save games: {e}"),
                                Err(e) => warn!("Steam refresh: save task join error: {e}"),
                            }
                        }
                    }
                    None => {
                        info!("Steam refresh: Steam not found or no games installed");
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        // ── Custom commands ────────────────────────────────────────────
        if execute_custom_command(state, name).await? {
            return Ok(());
        }

        // DiscordJoin is subscribed whenever the discord feature is on but has no
        // inline arm, so its payload falls through to the launcher resolver. It
        // must ONLY carry a discord deep-link; otherwise a steam:/epic:/close:/
        // kill: payload (which is_arbitrary_launch doesn't gate) would run here,
        // bypassing the launch_game/close_game feature gates. Mirrors Windows.
        if name == "DiscordJoin"
            && !payload
                .trim()
                .to_ascii_lowercase()
                .starts_with("url:discord://")
        {
            warn!("Blocked non-discord DiscordJoin payload");
            return Ok(());
        }

        // Authorization: exe:/lnk:/url: payloads run an arbitrary program or URL,
        // which would defeat the allow_raw_commands=false guarantee (the launcher
        // shortcut path is otherwise "always allowed"). Only run them if they
        // match a configured game's launch command or raw commands are enabled.
        if crate::commands::is_arbitrary_launch(payload) {
            let cfg = state.config.read().await;
            if !cfg.allow_raw_commands && !crate::commands::is_configured_launch(&cfg, payload) {
                warn!(
                    "Blocked unconfigured launch payload for '{}' (add it as a game or enable allow_raw_commands)",
                    name
                );
                return Ok(());
            }
        }

        // ── Shell commands (predefined → launcher → raw → not found) ─────────────
        let cmd_str = match get_predefined_command(name) {
            Some(cmd) => cmd.to_string(),
            None => {
                // Try launcher shortcuts (always allowed - validated and safe)
                if let Some(expanded) = expand_launcher_shortcut(payload) {
                    expanded
                } else if !payload.is_empty() {
                    let config = state.config.read().await;
                    if !config.allow_raw_commands {
                        warn!("Raw command blocked (allow_raw_commands=false): {}", name);
                        return Ok(());
                    }
                    payload.to_string()
                } else {
                    warn!("No command configured for: {}", name);
                    return Ok(());
                }
            }
        };

        // For steam:// launches, make sure Steam is up first (a WoL cold boot may
        // not have it running yet, and xdg-open steam://rungameid into an absent
        // Steam is silently dropped). Mirrors the Windows executor.
        if payload.starts_with("steam:")
            || payload.starts_with("update:")
            || payload.starts_with("validate:")
        {
            wait_for_steam().await;
        }

        info!("Running: {}", cmd_str);

        // Execute via bash in its own process group so a timeout can kill the
        // whole tree (equivalent to taskkill /T on Windows), not just bash.
        let mut child = Command::new("bash")
            .args(["-c", &cmd_str])
            .process_group(0)
            .spawn()?;
        let pid = child.id();

        // Wait with timeout in background
        tokio::spawn(async move {
            match tokio::time::timeout(
                std::time::Duration::from_mins(5),
                tokio::task::spawn_blocking(move || child.wait()),
            )
            .await
            {
                Ok(Ok(Ok(status))) => {
                    if !status.success() {
                        warn!("Command exited with: {}", status);
                    }
                }
                Ok(Ok(Err(e))) => error!("Command wait error: {}", e),
                Ok(Err(e)) => error!("Task join error: {}", e),
                Err(_) => {
                    warn!(
                        "Command timed out after 5 minutes, killing process group (PID {})",
                        pid
                    );
                    // Negative PID targets the whole process group.
                    let _ = Command::new("kill")
                        .args(["-KILL", &format!("-{pid}")])
                        .status();
                }
            }
        });

        Ok(())
    }
}

/// Close every currently-running configured game via the `close:` launcher
/// (SIGTERM), matching exactly what the running-game sensor reports. Linux has
/// no process watcher, so we read `/proc` once on a blocking thread.
async fn close_running_games(state: &Arc<AppState>) {
    let names = tokio::task::spawn_blocking(crate::sensors::current_process_names)
        .await
        .unwrap_or_default();
    let running = {
        let config = state.config.read().await;
        config.matching_game_processes(names.iter().map(String::as_str))
    };
    if running.is_empty() {
        info!("CloseGame: no running game detected");
        return;
    }
    for proc in running {
        let Some(cmd) = expand_launcher_shortcut(&format!("close:{proc}")) else {
            continue;
        };
        info!("CloseGame: closing {}", proc);
        // .status() blocks briefly (pkill returns fast) and reaps the child so
        // we don't leak a zombie; run it off the async runtime.
        let handle =
            tokio::task::spawn_blocking(move || Command::new("bash").args(["-c", &cmd]).status());
        if let Ok(Err(e)) = handle.await {
            warn!("CloseGame: failed to run close command: {}", e);
        }
    }
}

/// Best-effort: ensure Steam is running before a steam:// launch. On a WoL cold
/// boot Steam may not be up yet, and `xdg-open steam://rungameid` into an absent
/// Steam is silently dropped. Starts Steam if needed and waits for it, then
/// proceeds regardless (the protocol handler may still bring it up).
async fn wait_for_steam() {
    use std::time::Duration;

    if tokio::task::spawn_blocking(steam_running)
        .await
        .unwrap_or(false)
    {
        debug!("Steam already running");
        return;
    }

    info!("Steam not running, starting it before launch...");
    let _ = tokio::task::spawn_blocking(|| {
        std::process::Command::new("xdg-open")
            .arg("steam://open/main")
            .status()
    })
    .await;

    info!("Waiting for Steam to finish starting...");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(STEAM_WAIT_TIMEOUT_SECS);
    while tokio::time::Instant::now() < deadline {
        if tokio::task::spawn_blocking(steam_running)
            .await
            .unwrap_or(false)
        {
            tokio::time::sleep(Duration::from_secs(STEAM_INIT_DELAY_SECS)).await;
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    warn!(
        "Steam not detected after {}s, proceeding with launch anyway",
        STEAM_WAIT_TIMEOUT_SECS
    );
}

/// Whether a Steam client process is running (scans /proc comm).
fn steam_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else {
            continue;
        };
        if !s.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            let comm = comm.trim();
            if comm.eq_ignore_ascii_case("steam") || comm.eq_ignore_ascii_case("steam.sh") {
                return true;
            }
        }
    }
    false
}

/// Send a keybind via xdotool (Linux equivalent of Windows keybd_event).
///
/// Converts our format ("ctrl+f6") to xdotool format ("ctrl+F6").
fn send_keybind_linux(keybind: &str) {
    let xdotool_keybind: String = keybind
        .split('+')
        .map(|part| {
            let lower = part.trim().to_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => "ctrl".to_string(),
                "shift" => "shift".to_string(),
                "alt" => "alt".to_string(),
                "win" | "super" => "super".to_string(),
                // Function keys: xdotool expects uppercase F
                k if k.starts_with('f') && k[1..].parse::<u32>().is_ok() => k.to_uppercase(),
                k => k.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("+");

    info!("Sending keybind via xdotool: {}", xdotool_keybind);
    // .status() reaps the child immediately - xdotool returns once the key
    // event is queued (microseconds).  .spawn() alone would leak a zombie.
    match Command::new("xdotool")
        .args(["key", &xdotool_keybind])
        .status()
    {
        Ok(_) => {}
        Err(e) => warn!("Failed to send keybind via xdotool: {}", e),
    }
}
