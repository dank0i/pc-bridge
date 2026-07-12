//! Data model: feature groups, the feature record (with provenance + security
//! metadata), the registry, and the game library. This is the shape the real
//! agent registry will back when wired up.

use std::collections::HashMap;

use crate::config::GameConfig;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Mqtt,
    Native,
}

/// Sensor = the PC reports a value to HA. Action = HA can trigger something.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sensor,
    Action,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    General,
    Games,
    Hardware,
    Audio,
    Presence,
    Power,
    Notifications,
    Custom,
}

impl Group {
    pub const ALL: [Group; 8] = [
        Group::General,
        Group::Games,
        Group::Hardware,
        Group::Audio,
        Group::Presence,
        Group::Power,
        Group::Notifications,
        Group::Custom,
    ];

    pub fn index(self) -> usize {
        match self {
            Group::General => 0,
            Group::Games => 1,
            Group::Hardware => 2,
            Group::Audio => 3,
            Group::Presence => 4,
            Group::Power => 5,
            Group::Notifications => 6,
            Group::Custom => 7,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Group::General => "General",
            Group::Games => "Games",
            Group::Hardware => "Hardware",
            Group::Audio => "Audio & Media",
            Group::Presence => "Presence",
            Group::Power => "Power",
            Group::Notifications => "Notifications",
            Group::Custom => "Custom",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Group::General => "Connection, device, security, and app behavior.",
            Group::Games => "What you're playing, your library, and Steam downloads.",
            Group::Hardware => "Live telemetry: GPU, CPU, memory, disks, network.",
            Group::Audio => "Sound devices, volume, mic/cam, and media.",
            Group::Presence => "Whether you're at the machine and what's in focus.",
            Group::Power => "Power state plus shutdown, sleep, lock, and restart.",
            Group::Notifications => "Messages from Home Assistant to the PC.",
            Group::Custom => "Your own sensors and actions, built from any value.",
        }
    }
}

pub struct Feature {
    /// Stable id; used for entity naming once wired to the agent.
    #[allow(dead_code)]
    pub id: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
    pub group: Group,
    pub kind: Kind,
    /// Needs elevation or is destructive; gated behind the Security setting.
    pub privileged: bool,
    pub enabled: bool,
    pub status: Status,
    /// Sensor: current reported value. Action: the command it runs.
    pub value: String,
    /// Poll interval in seconds. 0 means the sensor is event-driven (no polling).
    pub interval: u32,
    /// HA entity id / MQTT object it publishes as.
    pub entity: String,
    /// A real, non-obvious prerequisite ("" = none). Drives the SETUP badge.
    pub requires: &'static str,
    /// One-line "how it works" ("" = obvious).
    pub method: &'static str,
    pub expanded: bool,
}

#[allow(clippy::too_many_arguments)]
fn s(
    id: &'static str,
    name: &'static str,
    desc: &'static str,
    group: Group,
    enabled: bool,
    status: Status,
    value: &str,
    interval: u32,
    entity: &'static str,
    requires: &'static str,
    method: &'static str,
) -> Feature {
    Feature {
        id,
        name,
        desc,
        group,
        kind: Kind::Sensor,
        privileged: false,
        enabled,
        status,
        value: value.to_owned(),
        interval,
        entity: entity.to_owned(),
        requires,
        method,
        expanded: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn a(
    id: &'static str,
    name: &'static str,
    desc: &'static str,
    group: Group,
    privileged: bool,
    enabled: bool,
    value: &str,
    entity: &'static str,
    requires: &'static str,
    method: &'static str,
) -> Feature {
    Feature {
        id,
        name,
        desc,
        group,
        kind: Kind::Action,
        privileged,
        enabled,
        status: Status::Running,
        value: value.to_owned(),
        interval: 0,
        entity: entity.to_owned(),
        requires,
        method,
        expanded: false,
    }
}

pub fn registry(device_id: &str) -> Vec<Feature> {
    use Group::{Audio, Games, Hardware, Notifications, Power, Presence};
    use Status::{Error, Running};
    // interval 0 = event-driven; >0 = polled every N seconds.
    // The entity strings below use "dank0i_pc" as a placeholder device id; it is
    // rewritten to the configured device below so the UI shows the user's real
    // HA entity ids (unique_id = "{device_id}_{name}" in discovery).
    let mut features = vec![
        // Games
        s(
            "running_game",
            "Running Game",
            "The game you're currently playing.",
            Games,
            true,
            Running,
            "Battlefield 6",
            0,
            "sensor.dank0i_pc_running_game",
            "",
            "Event-driven process detection (WMI)",
        ),
        s(
            "game_catalog",
            "Game Catalog",
            "The list of games HA can launch.",
            Games,
            true,
            Running,
            "47 games",
            0,
            "sensor.dank0i_pc_game_catalog",
            "",
            "Rebuilt when the library changes",
        ),
        s(
            "steam_library",
            "Steam Library Sync",
            "Auto-discovers installed Steam games.",
            Games,
            true,
            Running,
            "Steam 47 · Epic 3",
            0,
            "sensor.dank0i_pc_steam_library",
            "",
            "Watches libraryfolders.vdf and appinfo.vdf",
        ),
        s(
            "steam_downloads",
            "Steam Updating",
            "On/off when Steam is downloading or updating a game. Safe, no setup.",
            Games,
            true,
            Running,
            "on",
            0,
            "sensor.dank0i_pc_steam_updating",
            "",
            "Reads Steam's .acf files. No debug port, no restart needed.",
        ),
        a(
            "launch_game",
            "Launch Game",
            "Lets HA start a game.",
            Games,
            false,
            true,
            "steam://run or path",
            "button.dank0i_pc_launch_game",
            "",
            "steam:// URIs or shortcut path",
        ),
        a(
            "close_game",
            "Close Game",
            "Lets HA close a running game.",
            Games,
            false,
            true,
            "graceful window close",
            "button.dank0i_pc_close_game",
            "",
            "PowerShell CloseMainWindow()",
        ),
        // Hardware (polled telemetry)
        s(
            "gpu",
            "GPU",
            "Temperature, load, clocks, VRAM.",
            Hardware,
            true,
            Running,
            "RTX 4090 · 54C · 31%",
            5,
            "sensor.dank0i_pc_gpu",
            "",
            "NVML / driver query",
        ),
        s(
            "cpu",
            "CPU",
            "Temperature and load.",
            Hardware,
            true,
            Running,
            "9800X3D · 42C · 8%",
            5,
            "sensor.dank0i_pc_cpu",
            "",
            "OS performance counters",
        ),
        s(
            "memory",
            "Memory",
            "RAM usage.",
            Hardware,
            true,
            Running,
            "18.2 / 32 GB",
            10,
            "sensor.dank0i_pc_memory",
            "",
            "",
        ),
        s(
            "disks",
            "Disks",
            "Free space per drive.",
            Hardware,
            false,
            Running,
            "C 41% · D 72%",
            30,
            "sensor.dank0i_pc_disks",
            "",
            "",
        ),
        s(
            "network",
            "Network",
            "Throughput up and down.",
            Hardware,
            true,
            Error,
            "adapter not found",
            5,
            "sensor.dank0i_pc_network",
            "",
            "NIC counters",
        ),
        s(
            "uptime",
            "Uptime",
            "Time since last boot.",
            Hardware,
            true,
            Running,
            "3d 4h",
            60,
            "sensor.dank0i_pc_uptime",
            "",
            "",
        ),
        s(
            "hwinfo",
            "HWiNFO Bridge",
            "CPU/GPU temps, power, clocks & fans (HWiNFO).",
            Hardware,
            false,
            Running,
            "gpu_temp 64 C",
            5,
            "sensor.dank0i_pc_hwinfo_*",
            "HWiNFO open with 'Shared Memory Support' enabled (Settings)",
            "Publishes ~21 mapped sensors: cpu/gpu package+hotspot+memory temps, cpu/gpu/soc power, cpu/gpu core+memory clocks, cpu/gpu load, vram %, gpu+case fans, VRM temp, framerate. Reads HWiNFO's shared memory.",
        ),
        // Audio & Media (event-driven)
        s(
            "audio_device",
            "Default Audio Device",
            "Current output device.",
            Audio,
            true,
            Running,
            "Speakers",
            0,
            "sensor.dank0i_pc_audio_device",
            "",
            "Device-change notifications",
        ),
        s(
            "volume",
            "Volume",
            "System output volume.",
            Audio,
            true,
            Running,
            "40%",
            0,
            "sensor.dank0i_pc_volume",
            "",
            "Endpoint volume notifications",
        ),
        s(
            "now_playing",
            "Now Playing",
            "Active media session.",
            Audio,
            false,
            Running,
            "Spotify · idle",
            0,
            "sensor.dank0i_pc_now_playing",
            "",
            "System media transport (GSMTC)",
        ),
        s(
            "mic",
            "Microphone In Use",
            "Whether the mic is active.",
            Audio,
            false,
            Running,
            "no",
            0,
            "binary_sensor.dank0i_pc_mic",
            "",
            "Capture-device activity",
        ),
        s(
            "webcam",
            "Webcam In Use",
            "Whether the camera is active.",
            Audio,
            false,
            Running,
            "no",
            0,
            "binary_sensor.dank0i_pc_webcam",
            "",
            "Camera-device activity",
        ),
        a(
            "media_controls",
            "Media Controls",
            "Play, pause, and skip from HA.",
            Audio,
            false,
            false,
            "play / pause / next",
            "button.dank0i_pc_media_*",
            "",
            "System media transport (GSMTC)",
        ),
        // Presence
        s(
            "idle",
            "Idle / Last Active",
            "Whether you're at the keyboard.",
            Presence,
            true,
            Running,
            "active",
            5,
            "binary_sensor.dank0i_pc_active",
            "",
            "Input idle time",
        ),
        s(
            "active_window",
            "Active Window",
            "Foreground application.",
            Presence,
            false,
            Running,
            "VS Code",
            0,
            "sensor.dank0i_pc_active_window",
            "",
            "Foreground-window change events",
        ),
        s(
            "session",
            "Session State",
            "Locked, unlocked, or away.",
            Presence,
            true,
            Running,
            "unlocked",
            0,
            "sensor.dank0i_pc_session",
            "",
            "Session notifications",
        ),
        // Power (event-driven state + actions)
        s(
            "sleep_wake",
            "Sleep / Wake",
            "Sleep state and wake events.",
            Power,
            true,
            Running,
            "awake",
            0,
            "sensor.dank0i_pc_sleep_state",
            "",
            "Power-broadcast events",
        ),
        s(
            "display_state",
            "Display State",
            "Whether the monitors are on.",
            Power,
            true,
            Running,
            "on",
            0,
            "binary_sensor.dank0i_pc_display",
            "",
            "Display power notifications",
        ),
        a(
            "shutdown",
            "Shutdown",
            "Power the PC off.",
            Power,
            true,
            true,
            "shutdown /s /t 0",
            "button.dank0i_pc_shutdown",
            "",
            "shutdown.exe",
        ),
        a(
            "restart",
            "Restart",
            "Reboot the PC.",
            Power,
            true,
            true,
            "shutdown /r /t 0",
            "button.dank0i_pc_restart",
            "",
            "shutdown.exe",
        ),
        a(
            "sleep",
            "Sleep",
            "Suspend the PC.",
            Power,
            false,
            true,
            "suspend",
            "button.dank0i_pc_sleep",
            "",
            "SetSuspendState",
        ),
        a(
            "lock",
            "Lock",
            "Lock the session.",
            Power,
            false,
            true,
            "lock workstation",
            "button.dank0i_pc_lock",
            "",
            "LockWorkStation",
        ),
        a(
            "logoff",
            "Log Off",
            "Sign out the user.",
            Power,
            true,
            false,
            "shutdown /l",
            "button.dank0i_pc_logoff",
            "",
            "shutdown /l",
        ),
        a(
            "monitor",
            "Monitor On / Off",
            "Turn displays on or off.",
            Power,
            false,
            true,
            "display power",
            "button.dank0i_pc_monitor",
            "",
            "Monitor power message",
        ),
        // Notifications
        a(
            "notifications",
            "HA Notifications",
            "Show Home Assistant messages on the PC.",
            Notifications,
            false,
            false,
            "native toast",
            "notify.dank0i_pc",
            "",
            "Native notification API",
        ),
        // NOTE: Custom actions/sensors are NOT in this registry - they're
        // user-defined and rendered from config.custom_commands / custom_sensors
        // in the Custom tab. Placeholder entries here were never shown but still
        // counted toward the "N active" total, so they were removed.
    ];
    if device_id != "dank0i_pc" {
        for f in &mut features {
            f.entity = f.entity.replace("dank0i_pc", device_id);
        }
    }
    features
}

// ---- game library ----

// Non-Steam launcher variants are recognized by the UI but not yet produced by
// the config mapping (only Steam/Manual are derivable from GameConfig today).
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    Steam,
    Epic,
    Xbox,
    Gog,
    Manual,
}

impl Launcher {
    pub fn tag(self) -> &'static str {
        match self {
            Launcher::Steam => "STEAM",
            Launcher::Epic => "EPIC",
            Launcher::Xbox => "XBOX",
            Launcher::Gog => "GOG",
            Launcher::Manual => "MANUAL",
        }
    }
}

// Live game states (running/downloading/update) come from runtime data the UI
// will show once wired to the agent; the config mapping reports Installed.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GameStatus {
    Running,
    UpdatePending,
    Installed,
}

#[derive(Clone)]
pub struct Game {
    pub name: String,
    pub process: String,
    pub path: String,
    pub appid: u32,
    pub launcher: Launcher,
    pub status: GameStatus,
    pub exposed: bool,
    /// The agent's game id (matches the live `runninggames` sensor). Display-only;
    /// derived from the config on save, so it isn't read back by `library_to_games`.
    #[allow(clippy::struct_field_names)]
    pub game_id: String,
}

/// Strip a launcher scheme (`lnk:`/`exe:`/`url:`) so the UI shows just the raw path
/// or URL. Other/no scheme is returned unchanged.
pub(crate) fn strip_launch_scheme(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    for scheme in ["lnk:", "exe:", "url:"] {
        if lower.starts_with(scheme) {
            return s[scheme.len()..].to_string();
        }
    }
    s.to_string()
}

/// Re-apply the launcher scheme when folding a manual game's edited path back into
/// the config: a URL gets `url:`, a `.lnk` gets `lnk:`, anything else `exe:`. If the
/// user already typed a scheme, it's left as-is.
fn add_launch_scheme(path: &str) -> String {
    let p = path.trim();
    if p.is_empty() {
        return String::new();
    }
    let lower = p.to_ascii_lowercase();
    if ["lnk:", "exe:", "url:", "steam:", "epic:"]
        .iter()
        .any(|s| lower.starts_with(s))
    {
        return p.to_string();
    }
    if p.contains("://") {
        format!("url:{p}")
    } else if lower.ends_with(".lnk") {
        format!("lnk:{p}")
    } else {
        format!("exe:{p}")
    }
}

/// Derive a stable game_id (snake_case, ascii-alphanumeric) from a display name.
fn game_id_from_name(name: &str) -> String {
    // Delegate to the single canonical slug (config::slugify_game_id) so the UI
    // and Steam discovery can never diverge again.
    crate::config::slugify_game_id(name)
}

/// Build the editable library view from the real `games` config map.
/// Sorted by name so the list is stable across launches.
pub fn games_to_library(games: &HashMap<String, GameConfig>) -> Vec<Game> {
    let mut v: Vec<Game> = games
        .iter()
        .map(|(process, gc)| {
            let appid = gc.app_id().unwrap_or(0);
            Game {
                name: gc.display_name(),
                process: process.clone(),
                // Steam games launch from app_id; manual games carry an explicit
                // launch command in this field.
                path: if appid != 0 {
                    String::new()
                } else {
                    // Show just the path/URL; the lnk:/exe:/url: scheme is re-added
                    // on save (see library_to_games).
                    strip_launch_scheme(&gc.launch_command().unwrap_or_default())
                },
                appid,
                launcher: if appid != 0 {
                    Launcher::Steam
                } else {
                    Launcher::Manual
                },
                status: GameStatus::Installed,
                exposed: gc.is_exposed(),
                game_id: gc.game_id().to_string(),
            }
        })
        .collect();
    v.sort_by_key(|g| g.name.to_lowercase());
    v
}

/// Fold the edited library back into a `games` config map. Entries with a blank
/// process name are dropped (incomplete rows). `prev` preserves the
/// `auto_discovered` flag for games already known to Steam discovery so a UI
/// save does not make them look manually added.
pub fn library_to_games(
    library: &[Game],
    prev: &HashMap<String, GameConfig>,
) -> HashMap<String, GameConfig> {
    library
        .iter()
        // Drop incomplete rows: a blank process has no detection key, a blank
        // name would derive an empty game_id.
        .filter(|g| !g.process.trim().is_empty() && !g.name.trim().is_empty())
        .map(|g| {
            let launch_command = if g.appid != 0 || g.path.trim().is_empty() {
                None
            } else {
                // The UI shows the bare path; re-apply the lnk:/exe:/url: scheme.
                Some(add_launch_scheme(g.path.trim()))
            };
            // One lookup on the trimmed process (the map's key) for both fields.
            let prev_gc = prev.get(g.process.trim());
            let auto_discovered = prev_gc.map(GameConfig::is_auto_discovered).unwrap_or(false);
            // Keep the stored game_id stable across edits: it is the HA entity id
            // (and its HomeKit accessory + Live Activity state). Re-deriving it from
            // the name on every save churned entities whenever the two slug rules
            // disagreed on punctuation ("Baldur's Gate 3" -> baldurs_gate_3 from Steam
            // discovery vs baldur_s_gate_3 here). Only derive an id for a NEW row.
            let game_id = prev_gc
                .map(|gc| gc.game_id().to_string())
                .unwrap_or_else(|| {
                    // New row: derive from the name, but fall back to the process (the
                    // non-empty map key) so an all-punctuation name can't yield a blank
                    // id, matching merge_steam_games' empty-slug guard.
                    let id = game_id_from_name(&g.name);
                    if id.is_empty() {
                        let from_proc = game_id_from_name(&g.process);
                        if from_proc.is_empty() {
                            "game".to_string()
                        } else {
                            from_proc
                        }
                    } else {
                        id
                    }
                });
            let gc = GameConfig::Full {
                game_id,
                app_id: if g.appid != 0 { Some(g.appid) } else { None },
                name: Some(g.name.trim().to_string()),
                launch_command,
                auto_discovered,
                exposed: g.exposed,
            };
            (g.process.trim().to_string(), gc)
        })
        .collect()
}

/// Merge an externally-updated on-disk config into the one this window is about to
/// save, so a settings save doesn't clobber a change the agent made while the
/// window was open - chiefly a Steam scan triggered from Home Assistant.
///
/// `baseline_keys` are the game keys this window knew about (its in-memory config
/// before folding the edited library). A fresh game key we never had is re-added;
/// the user's edits to games they could actually see still win. Fields the UI
/// never edits are taken from disk so an external change to them survives.
pub fn reconcile_with_disk(
    cfg: &mut crate::config::Config,
    fresh: crate::config::Config,
    baseline_keys: &std::collections::HashSet<String>,
) {
    // Fields the settings UI never writes: keep the on-disk values.
    cfg.disk_sensor_paths = fresh.disk_sensor_paths;
    cfg.discord_keybind = fresh.discord_keybind;
    cfg.allow_raw_commands = fresh.allow_raw_commands;
    cfg.mqtt.client_id = fresh.mqtt.client_id;
    // Re-add games discovered externally since this window loaded (keys we never
    // had). `or_insert` never overwrites a game the user is actively editing.
    for (key, gc) in fresh.games {
        if !baseline_keys.contains(&key) {
            cfg.games.entry(key).or_insert(gc);
        }
    }
    // NOTE: `removed_games` is intentionally NOT reconciled from disk. The settings
    // UI is its sole writer (the agent's Steam refresh only reads the suppression,
    // never adds to it), so the in-memory list is authoritative. Unioning the disk
    // copy back in would re-suppress a game the user just restored.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_game_id_from_name() {
        assert_eq!(game_id_from_name("Battlefield 6"), "battlefield_6");
        assert_eq!(
            game_id_from_name("The Witcher 3: Wild Hunt"),
            "the_witcher_3_wild_hunt"
        );
        assert_eq!(game_id_from_name("  Spaced  Out  "), "spaced_out");
    }

    #[test]
    fn test_library_to_games_preserves_stored_id() {
        // The stored id and what the slug derives from the name differ here: Steam
        // discovery historically stripped punctuation ("baldurs_gate_3"); the slug
        // now yields "baldur_s_gate_3". Editing in the UI must NOT re-slug it.
        assert_ne!(game_id_from_name("Baldur's Gate 3"), "baldurs_gate_3");

        let mut prev = HashMap::new();
        prev.insert(
            "bg3.exe".to_string(),
            GameConfig::Full {
                game_id: "baldurs_gate_3".to_string(),
                app_id: Some(1_086_940),
                name: Some("Baldur's Gate 3".to_string()),
                launch_command: None,
                auto_discovered: true,
                exposed: true,
            },
        );

        // Round-trip through the editable library and back preserves the id.
        let out = library_to_games(&games_to_library(&prev), &prev);
        let gc = out.get("bg3.exe").expect("game preserved by process key");
        assert_eq!(
            gc.game_id(),
            "baldurs_gate_3",
            "stored id must survive an edit"
        );
        assert!(gc.is_auto_discovered(), "auto_discovered must survive too");

        // A brand-new row (absent from prev) still derives its id from the name.
        let mut fresh = HashMap::new();
        fresh.insert(
            "new.exe".to_string(),
            GameConfig::Full {
                game_id: "ignored".to_string(),
                app_id: None,
                name: Some("New Game".to_string()),
                launch_command: None,
                auto_discovered: false,
                exposed: true,
            },
        );
        let new_out = library_to_games(&games_to_library(&fresh), &HashMap::new());
        assert_eq!(new_out.get("new.exe").unwrap().game_id(), "new_game");
    }

    #[test]
    fn test_library_round_trip_preserves_auto_discovered() {
        let mut games = HashMap::new();
        games.insert(
            "bf6.exe".to_string(),
            GameConfig::from_steam("battlefield_6".into(), 2_807_960, "Battlefield 6".into()),
        );
        games.insert(
            "manual.exe".to_string(),
            GameConfig::Full {
                game_id: "my_game".into(),
                app_id: None,
                name: Some("My Game".into()),
                launch_command: Some("exe:C:/game.exe".into()),
                auto_discovered: false,
                exposed: false,
            },
        );

        let lib = games_to_library(&games);
        assert_eq!(lib.len(), 2);
        // Sorted by name: "Battlefield 6" before "My Game".
        assert_eq!(lib[0].name, "Battlefield 6");
        assert_eq!(lib[0].appid, 2_807_960);
        assert!(matches!(lib[0].launcher, Launcher::Steam));
        assert_eq!(lib[1].name, "My Game");
        // Path is shown without the scheme; library_to_games re-adds exe:/lnk:/url:.
        assert_eq!(lib[1].path, "C:/game.exe");
        assert!(!lib[1].exposed);

        let back = library_to_games(&lib, &games);
        assert_eq!(back.len(), 2);
        assert_eq!(back.get("bf6.exe").unwrap().app_id(), Some(2_807_960));
        // Steam game keeps its auto_discovered flag across a UI save.
        assert!(back.get("bf6.exe").unwrap().is_auto_discovered());
        assert!(!back.get("manual.exe").unwrap().is_auto_discovered());
        assert_eq!(
            back.get("manual.exe").unwrap().launch_command(),
            Some("exe:C:/game.exe".to_string())
        );
    }

    #[test]
    fn test_library_drops_blank_process_rows() {
        let row = |name: &str, process: &str| Game {
            name: name.into(),
            process: process.into(),
            path: String::new(),
            appid: 0,
            launcher: Launcher::Manual,
            status: GameStatus::Installed,
            exposed: true,
            game_id: String::new(),
        };
        // Blank process (no detection key) and blank name (empty game_id) both drop.
        let lib = vec![row("x", "   "), row("  ", "game.exe")];
        assert!(library_to_games(&lib, &HashMap::new()).is_empty());
        // A complete row survives.
        assert_eq!(
            library_to_games(&[row("Doom", "doom.exe")], &HashMap::new()).len(),
            1
        );
    }

    #[test]
    fn test_reconcile_with_disk_preserves_external_changes() {
        use crate::config::Config;
        // The window loaded knowing only "doom". While it was open the agent's Steam
        // scan added "cs2" and set a disk path. Our stale save must not wipe those.
        let baseline: std::collections::HashSet<String> =
            ["doom".to_string()].into_iter().collect();

        let mut ours = Config::default();
        ours.games
            .insert("doom".to_string(), GameConfig::Simple("doom".into()));

        let mut fresh = Config::default();
        fresh
            .games
            .insert("doom".to_string(), GameConfig::Simple("doom".into()));
        fresh.games.insert(
            "cs2".to_string(),
            GameConfig::from_steam("cs2".into(), 730, "CS2".into()),
        );
        fresh.disk_sensor_paths = vec!["C:\\".to_string()];
        // The disk copy of removed_games must be IGNORED (UI is the sole owner), or
        // a game the user just restored (dropped from the in-memory list) would be
        // re-suppressed from the stale disk copy.
        fresh.removed_games = vec![crate::config::RemovedGame {
            process: "old_game".to_string(),
            name: "Old Game".to_string(),
            game_id: None,
            app_id: None,
        }];

        reconcile_with_disk(&mut ours, fresh, &baseline);

        // Externally-added game survived; our known game untouched.
        assert!(ours.games.contains_key("cs2"), "external scan must survive");
        assert!(ours.games.contains_key("doom"));
        // UI-untouched field adopted from disk.
        assert_eq!(ours.disk_sensor_paths, vec!["C:\\".to_string()]);
        // Suppression list is UI-owned: the disk copy is not merged in.
        assert!(
            ours.removed_games.is_empty(),
            "removed_games must not be pulled from disk"
        );
    }

    #[test]
    fn test_reconcile_does_not_revive_user_removed_game() {
        use crate::config::Config;
        // A game the user just removed IS in the baseline (the window knew it), so a
        // fresh copy still on disk must NOT be re-added over the removal.
        let baseline: std::collections::HashSet<String> = ["doom".to_string(), "cs2".to_string()]
            .into_iter()
            .collect();

        let mut ours = Config::default(); // user removed both; empty games
        let mut fresh = Config::default();
        fresh
            .games
            .insert("cs2".to_string(), GameConfig::Simple("cs2".into()));

        reconcile_with_disk(&mut ours, fresh, &baseline);
        assert!(
            !ours.games.contains_key("cs2"),
            "a removal of a known game must not be undone by the reconcile"
        );
    }
}
