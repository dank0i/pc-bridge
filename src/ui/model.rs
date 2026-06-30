//! Data model: feature groups, the feature record (with provenance + security
//! metadata), the registry, and the game library. This is the shape the real
//! agent registry will back when wired up.

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
    pub entity: &'static str,
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
        entity,
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
        entity,
        requires,
        method,
        expanded: false,
    }
}

pub fn registry() -> Vec<Feature> {
    use Group::{Audio, Custom, Games, Hardware, Notifications, Power, Presence};
    use Status::{Error, Running};
    // interval 0 = event-driven; >0 = polled every N seconds.
    vec![
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
            "Steam Downloads",
            "Live download %, queue, and speed.",
            Games,
            true,
            Running,
            "Apex Legends · 76%",
            0,
            "sensor.dank0i_pc_steam_downloads",
            "",
            "Subscribes to Steam's CEF debug API",
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
            "Imports any sensor from HWiNFO.",
            Hardware,
            false,
            Running,
            "62 sensors",
            5,
            "sensor.dank0i_pc_hwinfo_*",
            "HWiNFO running with Shared Memory (Pro)",
            "Reads HWiNFO shared memory",
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
        // Custom (Actions + Sensors)
        a(
            "custom_cmd",
            "Custom Command",
            "Run any command you define.",
            Custom,
            true,
            false,
            "",
            "button.dank0i_pc_custom",
            "",
            "Shell exec, review carefully",
        ),
        a(
            "launch_url",
            "Launch URL",
            "Open a link or app.",
            Custom,
            false,
            false,
            "",
            "button.dank0i_pc_launch_url",
            "",
            "ShellExecute",
        ),
        s(
            "custom_frametime",
            "Frametime (example)",
            "A custom sensor sourced from HWiNFO.",
            Custom,
            false,
            Running,
            "8.3 ms",
            1,
            "sensor.dank0i_pc_frametime",
            "HWiNFO Bridge enabled",
            "User-defined from a HWiNFO reading",
        ),
    ]
}

// ---- game library ----

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GameStatus {
    Running,
    Downloading(u8),
    UpdatePending,
    Installed,
}

pub struct Game {
    pub name: String,
    pub process: String,
    pub path: String,
    pub appid: u32,
    pub launcher: Launcher,
    pub status: GameStatus,
    pub exposed: bool,
}

pub fn library() -> Vec<Game> {
    use GameStatus::{Downloading, Installed, Running, UpdatePending};
    use Launcher::{Epic, Gog, Manual, Steam, Xbox};
    let g = |name: &str, process: &str, path: &str, appid, launcher, status, exposed| Game {
        name: name.to_owned(),
        process: process.to_owned(),
        path: path.to_owned(),
        appid,
        launcher,
        status,
        exposed,
    };
    vec![
        g(
            "Battlefield 6",
            "bf6.exe",
            r"D:\SteamLibrary\steamapps\common\Battlefield 6",
            2_807_960,
            Steam,
            Running,
            true,
        ),
        g(
            "Apex Legends",
            "r5apex.exe",
            r"D:\SteamLibrary\steamapps\common\Apex Legends",
            1_172_470,
            Steam,
            Downloading(76),
            true,
        ),
        g(
            "Marvel Rivals",
            "MarvelRivals.exe",
            r"D:\SteamLibrary\steamapps\common\MarvelRivals",
            2_767_030,
            Steam,
            Installed,
            true,
        ),
        g(
            "Deadlock",
            "deadlock.exe",
            r"D:\SteamLibrary\steamapps\common\Deadlock",
            1_422_450,
            Steam,
            UpdatePending,
            true,
        ),
        g(
            "Fortnite",
            "FortniteClient-Win64-Shipping.exe",
            r"C:\Program Files\Epic Games\Fortnite",
            0,
            Epic,
            Installed,
            true,
        ),
        g(
            "Forza Horizon 5",
            "ForzaHorizon5.exe",
            r"C:\XboxGames\Forza Horizon 5",
            0,
            Xbox,
            Installed,
            false,
        ),
        g(
            "Cyberpunk 2077",
            "Cyberpunk2077.exe",
            r"D:\SteamLibrary\steamapps\common\Cyberpunk 2077",
            1_091_500,
            Steam,
            Installed,
            true,
        ),
        g(
            "The Witcher 3",
            "witcher3.exe",
            r"C:\GOG Games\The Witcher 3",
            0,
            Gog,
            Installed,
            true,
        ),
        g(
            "retroarch (manual)",
            "retroarch.exe",
            r"C:\RetroArch",
            0,
            Manual,
            Installed,
            false,
        ),
    ]
}
