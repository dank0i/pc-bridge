//! App state and all views. A group/subsection master toggle (next to the name)
//! is a bulk switch: it is derived from the features (on if any is enabled) and
//! writes its new state through to every feature in the group, so what you see is
//! what gets saved. Uniform spacing via the theme scale. No em-dashes anywhere.

#![allow(clippy::too_many_lines)]

use eframe::egui;
use egui::{Color32, RichText, Rounding};

use crate::config::{
    Config, CustomCommand, CustomCommandType, CustomSensor, CustomSensorType, FeatureConfig,
};

use super::model::{
    Feature, Game, GameStatus, Group, Kind, Launcher, Status, Transport, games_to_library,
    library_to_games, registry,
};
use super::theme::{
    ACCENT, AMBER, BG, BLOCK, GAP, GREEN, GREY, ORANGE, PAD_X, PAD_Y, PANEL, PURPLE, RED, ROW,
    ROW_HOVER, ROW_OFF, TEXT, TEXT_DIM, TIGHT, badge, dot, kv, labeled, section, toggle,
};

pub struct App {
    device: String,
    transport: Transport,
    mqtt_host: String,
    mqtt_port: String,
    mqtt_user: String,
    mqtt_pass: String,
    ha_token: String,
    show_secrets: bool,
    /// Fingerprint of the discrete (non-text) settings. Auto-save fires whenever it
    /// changes, so a toggle/interval/expose persists immediately with no Save click.
    toggle_sig: Option<String>,
    beta_updates: bool,
    allow_privileged: bool,
    allow_global_launch: bool,
    allow_global_close: bool,
    show_tray_icon: bool,
    selected: Group,
    search: String,
    show_library: bool,
    custom_tab: Kind,
    group_on: [bool; 8],
    custom_actions_on: bool,
    custom_sensors_on: bool,
    /// Editable copies of the user-defined custom entities, folded back into
    /// the config on save.
    custom_sensors: Vec<CustomSensor>,
    custom_commands: Vec<CustomCommand>,
    features: Vec<Feature>,
    library: Vec<Game>,
    /// Live view: subscribes to the broker for the agent's real state (running game,
    /// download %, availability), so the UI reflects reality, not the config.
    live: crate::ui::live::LiveView,
    /// The real on-disk config. Settings widgets read/write the App fields
    /// above; Save folds them back into this and persists.
    cfg: Config,
    /// Set when an existing config existed but failed to load (corrupt file,
    /// credential can't decrypt). While set, Save is disabled so we never
    /// overwrite the user's real config with blank defaults.
    load_error: Option<String>,
    /// Set to the reason when the last Save failed (e.g. validation rejected the
    /// device name), so the user sees why instead of a silent no-op.
    save_error: Option<String>,
    /// True after a successful Save, for a "Saved" acknowledgment (distinct from
    /// the broker "connected" status).
    saved: bool,
    /// Feature ids that can't work on this session (no X11 and no wlr protocol);
    /// their toggles are greyed out and forced off.
    unsupported: Vec<&'static str>,
    /// One-time alert shown when saved-on features were forced off as unsupported.
    unsupported_alert: Option<String>,
    /// The interval groups as loaded, so Save writes back only the group whose
    /// slider the user actually changed (siblings share the value).
    orig_intervals: crate::config::IntervalConfig,
}

impl App {
    pub fn new() -> Self {
        // Load the real config. A first run (no file) is fine and starts from
        // defaults; a genuine load failure (corrupt file / undecryptable
        // credential) must NOT be shown as blank defaults, or Save would clobber
        // the real config and delete the credential file.
        let (cfg, load_error) = match Config::load() {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                if Config::is_first_run().unwrap_or(false) {
                    (Config::default(), None)
                } else {
                    (Config::default(), Some(format!("{e:#}")))
                }
            }
        };
        let (mqtt_host, mqtt_port) = split_broker(&cfg.mqtt.broker);
        let mut features = registry(&cfg.device_id());
        for f in &mut features {
            if let Some(on) = flag_get(&cfg.features, f.id) {
                f.enabled = on;
            }
            // Show the real poll interval from the shared group the feature uses
            // (the registry's per-feature default is just a placeholder).
            if f.interval > 0
                && let Some(field) = feature_interval_field(f.id)
            {
                f.interval = interval_field_get(&cfg.intervals, field);
            }
        }
        let orig_intervals = cfg.intervals.clone();

        // Features that can't work on this session (X11-only features on a
        // Wayland desktop) are forced off and greyed out; if any were enabled in
        // the saved config, alert once so the user knows they were disabled.
        let unsupported = unsupported_features();
        let mut forced_off = Vec::new();
        for f in &mut features {
            if unsupported.contains(&f.id) && f.enabled {
                f.enabled = false;
                forced_off.push(f.name);
            }
        }
        let unsupported_alert = (!forced_off.is_empty()).then(|| {
            format!(
                "Not supported on this session (these need an X11 desktop, not Wayland): {}. They've been turned off.",
                forced_off.join(", ")
            )
        });

        Self {
            device: cfg.device_name.clone(),
            transport: Transport::Mqtt,
            mqtt_host,
            mqtt_port,
            mqtt_user: cfg.mqtt.user.clone(),
            mqtt_pass: cfg.mqtt.pass.clone(),
            ha_token: String::new(),
            show_secrets: false,
            toggle_sig: None,
            beta_updates: cfg.update_channel == "beta",
            allow_privileged: cfg.custom_command_privileges_allowed,
            allow_global_launch: cfg.allow_global_launch,
            allow_global_close: cfg.allow_global_close,
            show_tray_icon: cfg.show_tray_icon,
            selected: Group::Games,
            search: String::new(),
            show_library: false,
            custom_tab: Kind::Action,
            group_on: [true; 8],
            custom_actions_on: cfg.custom_commands_enabled,
            custom_sensors_on: cfg.custom_sensors_enabled,
            custom_sensors: cfg.custom_sensors.clone(),
            custom_commands: cfg.custom_commands.clone(),
            features,
            library: games_to_library(&cfg.games),
            live: crate::ui::live::start(&cfg),
            cfg,
            load_error,
            save_error: None,
            saved: false,
            unsupported,
            unsupported_alert,
            orig_intervals,
        }
    }

    /// Fold the edited settings back into the config and persist them.
    /// A fingerprint of the discrete (non-text) settings so auto-save fires only when
    /// a toggle / interval / expose actually changes (not on text-field keystrokes).
    fn toggle_signature(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        for f in &self.features {
            // Only fingerprint an interval that actually has a backing config field,
            // so editing a non-persistable slider doesn't trigger a save that discards
            // the value.
            let interval = if feature_interval_field(f.id).is_some() {
                f.interval
            } else {
                0
            };
            let _ = write!(s, "{}={},{};", f.id, f.enabled, interval);
        }
        for (i, g) in self.library.iter().enumerate() {
            let _ = write!(s, "g{i}={};", g.exposed);
        }
        let _ = write!(
            s,
            "P{}L{}C{}T{}B{}A{}S{}",
            self.allow_privileged,
            self.allow_global_launch,
            self.allow_global_close,
            self.show_tray_icon,
            self.beta_updates,
            self.custom_actions_on,
            self.custom_sensors_on
        );
        s
    }

    fn save(&mut self) -> anyhow::Result<()> {
        self.persist(true)
    }

    /// Persist the config. With `include_text=false` (used by the toggle auto-save)
    /// the free-text fields - broker, credentials, device name, custom-entity bodies -
    /// are NOT rewritten from the edit buffers, so a background save can't clobber a
    /// half-typed password or re-encrypt the credential file on every toggle. Those
    /// persist only via the explicit Save button (`include_text=true`).
    fn persist(&mut self, include_text: bool) -> anyhow::Result<()> {
        if let Some(e) = &self.load_error {
            anyhow::bail!("refusing to overwrite a config that failed to load: {e}");
        }
        // Features unsupported on THIS session (X11-only under Wayland) are forced
        // off in the UI; don't persist that forced-off state, or a user who opens
        // the settings once under Wayland would permanently lose those features
        // even back on X11. Leave their saved flag untouched.
        let unsupported = unsupported_features();
        for f in &self.features {
            if unsupported.contains(&f.id) {
                continue;
            }
            // Persist each feature's own state. The group master is a bulk switch
            // that already writes through to f.enabled (see the group header), so
            // there is no separate overlay to fold in here.
            flag_set(&mut self.cfg.features, f.id, f.enabled);
        }
        // Persist edited poll intervals back to their shared group. Only write the
        // group whose slider actually changed (all group members loaded the same
        // value, so an unedited sibling won't clobber the edit).
        for f in &self.features {
            if f.interval > 0
                && let Some(field) = feature_interval_field(f.id)
                && f.interval != interval_field_get(&self.orig_intervals, field)
            {
                interval_field_set(&mut self.cfg.intervals, field, f.interval);
            }
        }
        if include_text {
            self.cfg.device_name = self.device.clone();
            self.cfg.mqtt.broker = if self.mqtt_port.is_empty() {
                self.mqtt_host.clone()
            } else {
                format!("{}:{}", self.mqtt_host, self.mqtt_port)
            };
            self.cfg.mqtt.user = self.mqtt_user.clone();
            self.cfg.mqtt.pass = self.mqtt_pass.clone();
        }
        self.cfg.custom_command_privileges_allowed = self.allow_privileged;
        self.cfg.allow_global_launch = self.allow_global_launch;
        self.cfg.allow_global_close = self.allow_global_close;
        self.cfg.show_tray_icon = self.show_tray_icon;
        self.cfg.custom_commands_enabled = self.custom_actions_on;
        self.cfg.custom_sensors_enabled = self.custom_sensors_on;
        // A game's process is its detection KEY; two rows sharing one would
        // silently collapse (last-wins) in the map below, losing a row. Reject so
        // the user doesn't lose data on Save.
        {
            let mut seen = std::collections::HashSet::new();
            for g in &self.library {
                let key = g.process.trim();
                if !key.is_empty() && !seen.insert(key.to_ascii_lowercase()) {
                    anyhow::bail!(
                        "Two games share the process '{key}'. Each game needs a unique process."
                    );
                }
            }
        }
        // Fold the edited game library back into the config map, preserving the
        // Steam auto-discovered flag for games already known to discovery.
        self.cfg.games = library_to_games(&self.library, &self.cfg.games);
        // Custom-entity bodies are free text -> explicit Save only (see include_text).
        if include_text {
            // Persist edited custom entities, dropping rows with a blank name.
            self.cfg.custom_sensors = self
                .custom_sensors
                .iter()
                .filter(|s| !s.name.trim().is_empty())
                .cloned()
                .collect();
            self.cfg.custom_commands = self
                .custom_commands
                .iter()
                .filter(|c| !c.name.trim().is_empty())
                .cloned()
                .collect();
        }
        self.cfg.update_channel = if self.beta_updates {
            "beta".to_owned()
        } else if self.cfg.update_channel == "disabled" {
            "disabled".to_owned()
        } else {
            "stable".to_owned()
        };
        self.cfg.save()
    }

    /// Fold the live MQTT view into the games list each frame: mark the running game
    /// Running, the downloading game Downloading(%), everything else Installed.
    fn apply_live_state(&mut self) {
        let live = self.live.snapshot();
        // Normalize names (lowercase, alphanumeric-only) so "KovaaK's" matches
        // "KovaaKs" etc. when comparing the updating-games list to the library.
        let norm = |s: &str| -> String {
            s.chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_lowercase())
                .collect()
        };
        for g in &mut self.library {
            // pct 0 means idle (the sensor publishes "0" when not downloading), so
            // don't flash "Downloading 0%" as a finished download clears.
            let dl = if g.appid != 0 && live.download_appid == Some(g.appid) {
                live.download_pct.filter(|&p| p > 0)
            } else {
                None
            };
            // runninggames can be a comma-joined list when several are detected.
            let running = !g.game_id.is_empty()
                && live
                    .running_game_id
                    .as_deref()
                    .is_some_and(|s| s.split(',').any(|id| id == g.game_id));
            // steam_updating publishes updating games by display name.
            let gn = norm(&g.name);
            let updating = !gn.is_empty() && live.updating_games.iter().any(|n| norm(n) == gn);
            g.status = if let Some(p) = dl {
                GameStatus::Downloading(p)
            } else if running {
                GameStatus::Running
            } else if updating {
                GameStatus::UpdatePending
            } else {
                GameStatus::Installed
            };
        }
    }

    fn master_on(&self, f: &Feature) -> bool {
        if f.group == Group::Custom {
            match f.kind {
                Kind::Action => self.custom_actions_on,
                Kind::Sensor => self.custom_sensors_on,
            }
        } else {
            self.group_on[f.group.index()]
        }
    }

    fn effective(&self, f: &Feature) -> bool {
        self.master_on(f) && f.enabled && (!f.privileged || self.allow_privileged)
    }

    fn count(&self, g: Group) -> (usize, usize) {
        if g == Group::Custom {
            // Custom entities live in their own Vecs, not `features`; each set is
            // all-on or all-off via its master toggle.
            let total = self.custom_sensors.len() + self.custom_commands.len();
            let on = if self.custom_sensors_on {
                self.custom_sensors.len()
            } else {
                0
            } + if self.custom_actions_on {
                self.custom_commands.len()
            } else {
                0
            };
            return (on, total);
        }
        let it = self.features.iter().filter(|f| f.group == g);
        (it.clone().filter(|f| self.effective(f)).count(), it.count())
    }
}

fn in_view(f: &Feature, g: Group, ct: Kind) -> bool {
    if g == Group::Custom {
        f.group == Group::Custom && f.kind == ct
    } else {
        f.group == g
    }
}

/// Read the config flag backing a UI feature, if one exists yet.
///
/// Every UI feature that maps 1:1 onto a config flag is bound here. Ids that
/// have no backing flag (e.g. custom entries) return None and keep their
/// in-memory state.
/// Feature ids that can't work on the current session and must be forced off /
/// greyed out: the window/DPMS/monitor features when neither X11 nor the wlr
/// Wayland protocols are available (e.g. GNOME/KDE Wayland). Empty elsewhere.
fn unsupported_features() -> Vec<&'static str> {
    #[cfg(target_os = "linux")]
    {
        // A real X11 session (NOT XWayland under a Wayland compositor - that would
        // report x11 as reachable while the X11 backends only see XWayland): the
        // X11 backends handle everything.
        if !crate::linux_wayland::is_wayland_session() && crate::linux_x11::is_available() {
            return Vec::new();
        }
        // Wayland (or headless): window/DPMS/monitor need the wlr protocols.
        let mut out = Vec::new();
        if !crate::linux_wayland::has_foreign_toplevel() {
            out.push("active_window");
        }
        if !crate::linux_wayland::has_output_power() {
            out.push("display_state");
            out.push("monitor");
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// The shared IntervalConfig field a polled feature reads, if any. Several
/// features map to one field because the backend groups their sensor tasks
/// (e.g. gpu/cpu/memory/disk/network all share `system_sensors`), so editing one
/// feature's poll interval changes that whole group. `None` = event-driven or a
/// fixed interval with no configurable poll (e.g. uptime, hwinfo).
fn feature_interval_field(id: &str) -> Option<&'static str> {
    Some(match id {
        "gpu" => "gpu",
        "network" => "network",
        "disks" => "disk",
        "cpu" => "cpu",
        "memory" => "memory",
        "idle" => "last_active",
        // (steam downloads is event-driven, interval == 0, so it never reaches
        // this mapping - there's deliberately no arm for it.)
        "running_game" | "game_catalog" => "game_sensor",
        _ => return None,
    })
}

fn interval_field_get(iv: &crate::config::IntervalConfig, field: &str) -> u32 {
    let v = match field {
        "system_sensors" => iv.system_sensors,
        "cpu" => iv.cpu,
        "memory" => iv.memory,
        "gpu" => iv.gpu,
        "network" => iv.network,
        "disk" => iv.disk,
        "last_active" => iv.last_active,
        "steam_check" => iv.steam_check,
        "game_sensor" => iv.game_sensor,
        _ => 0,
    };
    v.min(u64::from(u32::MAX)) as u32
}

fn interval_field_set(iv: &mut crate::config::IntervalConfig, field: &str, v: u32) {
    let v = u64::from(v);
    match field {
        "system_sensors" => iv.system_sensors = v,
        "cpu" => iv.cpu = v,
        "memory" => iv.memory = v,
        "gpu" => iv.gpu = v,
        "network" => iv.network = v,
        "disk" => iv.disk = v,
        "last_active" => iv.last_active = v,
        "steam_check" => iv.steam_check = v,
        "game_sensor" => iv.game_sensor = v,
        _ => {}
    }
}

fn flag_get(f: &FeatureConfig, id: &str) -> Option<bool> {
    Some(match id {
        "gpu" => f.gpu_sensor,
        "network" => f.network_sensor,
        "disks" => f.disk_sensor,
        "uptime" => f.uptime_sensor,
        "hwinfo" => f.hwinfo_sensor,
        "cpu" => f.cpu_sensor,
        "memory" => f.memory_sensor,
        "active_window" => f.active_window,
        "session" => f.session_state,
        "audio_device" => f.audio_device,
        "mic" => f.mic,
        "webcam" => f.webcam,
        "now_playing" => f.now_playing,
        "idle" => f.idle_tracking,
        "running_game" => f.running_game,
        "game_catalog" => f.game_catalog,
        "steam_library" => f.steam_library,
        "launch_game" => f.launch_game,
        "close_game" => f.close_game,
        "volume" => f.volume,
        "media_controls" => f.media_controls,
        "steam_downloads" => f.steam_updates,
        "steam_download_progress" => f.steam_download_progress,
        "notifications" => f.notifications,
        "sleep_wake" => f.sleep_wake,
        "display_state" => f.display_state,
        "shutdown" => f.cmd_shutdown,
        "restart" => f.cmd_restart,
        "sleep" => f.cmd_sleep,
        "lock" => f.cmd_lock,
        "logoff" => f.cmd_logoff,
        "monitor" => f.cmd_monitor,
        _ => return None,
    })
}

fn flag_set(f: &mut FeatureConfig, id: &str, v: bool) {
    match id {
        "gpu" => f.gpu_sensor = v,
        "network" => f.network_sensor = v,
        "disks" => f.disk_sensor = v,
        "uptime" => f.uptime_sensor = v,
        "hwinfo" => f.hwinfo_sensor = v,
        "cpu" => f.cpu_sensor = v,
        "memory" => f.memory_sensor = v,
        "active_window" => f.active_window = v,
        "session" => f.session_state = v,
        "audio_device" => f.audio_device = v,
        "mic" => f.mic = v,
        "webcam" => f.webcam = v,
        "now_playing" => f.now_playing = v,
        "idle" => f.idle_tracking = v,
        "running_game" => f.running_game = v,
        "game_catalog" => f.game_catalog = v,
        "steam_library" => f.steam_library = v,
        "launch_game" => f.launch_game = v,
        "close_game" => f.close_game = v,
        "volume" => f.volume = v,
        "media_controls" => f.media_controls = v,
        "steam_downloads" => f.steam_updates = v,
        "steam_download_progress" => f.steam_download_progress = v,
        "notifications" => f.notifications = v,
        "sleep_wake" => f.sleep_wake = v,
        "display_state" => f.display_state = v,
        "shutdown" => f.cmd_shutdown = v,
        "restart" => f.cmd_restart = v,
        "sleep" => f.cmd_sleep = v,
        "lock" => f.cmd_lock = v,
        "logoff" => f.cmd_logoff = v,
        "monitor" => f.cmd_monitor = v,
        _ => {}
    }
}

/// Split an MQTT broker string into (host, port), defaulting the port to 1883.
fn split_broker(broker: &str) -> (String, String) {
    // Bracketed IPv6 with port: `[::1]:1883`.
    if broker.starts_with('[')
        && let Some((host, port)) = broker.rsplit_once(':')
        && host.ends_with(']')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        return (host.to_owned(), port.to_owned());
    }
    // A bare (unbracketed) IPv6 address has multiple ':' and no port - don't
    // misparse its last group as a port (e.g. `fe80::1`).
    if broker.matches(':').count() > 1 {
        return (broker.to_owned(), "1883".to_owned());
    }
    // Plain `host:port`.
    if let Some((host, port)) = broker.rsplit_once(':')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        return (host.to_owned(), port.to_owned());
    }
    (broker.to_owned(), "1883".to_owned())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Apply the live MQTT view (running game, download %) to the games list, and
        // keep repainting so the subscriber thread's updates show promptly.
        self.apply_live_state();
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        // Clear the "Saved" acknowledgement the moment the user edits anything, so
        // it never implies in-progress edits are already persisted. (Clicking Save
        // itself is a pointer press too, but save() re-sets `saved` afterward.)
        if self.saved
            && ctx.input(|i| {
                i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::Text(_)
                            | egui::Event::Key { pressed: true, .. }
                            | egui::Event::PointerButton { pressed: true, .. }
                    )
                })
            })
        {
            self.saved = false;
        }
        top_bar(self, ctx);
        side_rail(self, ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(18.0, 14.0)),
            )
            .show(ctx, |ui| {
                if let Some(msg) = self.unsupported_alert.clone() {
                    egui::Frame::none()
                        .fill(ROW_OFF)
                        .rounding(9.0)
                        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(msg).color(AMBER).size(13.0));
                                if ui.small_button("Dismiss").clicked() {
                                    self.unsupported_alert = None;
                                }
                            });
                        });
                    ui.add_space(BLOCK);
                }
                if self.selected == Group::General {
                    general_panel(self, ui);
                } else {
                    feature_panel(self, ui);
                }
            });

        // Auto-save the instant a toggle/interval/expose changes, so settings persist
        // without a Save click. Runs after the panel render, so this frame's edits are
        // already applied. Text fields (broker, credentials, game names) stay on
        // explicit Save - they're excluded from the signature so we don't write the
        // file + re-encrypt credentials on every keystroke.
        let sig = self.toggle_signature();
        match &self.toggle_sig {
            None => self.toggle_sig = Some(sig),
            Some(prev) if *prev != sig => match self.persist(false) {
                Ok(()) => {
                    self.save_error = None;
                    self.toggle_sig = Some(sig);
                }
                // Keep the OLD baseline so we retry once the underlying problem (e.g.
                // a duplicate game process) is fixed, rather than silently dropping the
                // change and pretending it was saved.
                Err(e) => self.save_error = Some(format!("{e:#}")),
            },
            _ => {}
        }
    }
}

fn top_bar(app: &App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("top")
        .frame(
            egui::Frame::none()
                .fill(PANEL)
                .inner_margin(egui::Margin::symmetric(16.0, 11.0)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("pc-bridge").strong().size(18.0).color(TEXT));
                ui.add_space(GAP);
                ui.label(RichText::new(&app.device).size(13.0).color(TEXT_DIM));
                // A save failure (e.g. duplicate game process) can originate on any
                // tab, so surface it globally here, not just on General.
                if let Some(e) = &app.save_error {
                    ui.add_space(GAP);
                    ui.label(RichText::new("· not saved").size(13.0).color(RED))
                        .on_hover_text(e);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // The settings window is a separate process from the agent, so it
                    // can't know the broker connection - but it CAN tell whether the
                    // background agent is running (via the singleton probe). Show that
                    // instead of a fake connection status.
                    let (txt, col) = if crate::instance_already_running() {
                        ("Agent running", GREEN)
                    } else {
                        ("Agent stopped", GREY)
                    };
                    ui.label(RichText::new(txt).color(col).size(13.0));
                    dot(ui, col, 4.5);
                    pipe(ui);
                    let tname = match app.transport {
                        Transport::Mqtt => "MQTT",
                        Transport::Native => "Native",
                    };
                    ui.label(RichText::new(tname).color(ACCENT).size(13.0).strong());
                    pipe(ui);
                    let active = app.features.iter().filter(|f| app.effective(f)).count();
                    ui.label(
                        RichText::new(format!("{active} / {} active", app.features.len()))
                            .color(TEXT_DIM)
                            .size(13.0),
                    );
                });
            });
        });
}

fn pipe(ui: &mut egui::Ui) {
    ui.add_space(GAP);
    ui.label(RichText::new("|").color(Color32::from_gray(0x3a)));
    ui.add_space(GAP);
}

fn side_rail(app: &mut App, ctx: &egui::Context) {
    egui::SidePanel::left("cats")
        .exact_width(190.0)
        .resizable(false)
        .frame(
            egui::Frame::none()
                .fill(PANEL)
                .inner_margin(egui::Margin::symmetric(10.0, 12.0)),
        )
        .show(ctx, |ui| {
            for g in Group::ALL {
                let selected = app.selected == g;
                let count = if g == Group::General {
                    None
                } else {
                    Some(app.count(g))
                };
                if nav_item(ui, g.name(), selected, count).clicked() {
                    app.selected = g;
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(TIGHT);
                ui.label(
                    RichText::new(concat!("v", env!("CARGO_PKG_VERSION"), "  ·  prototype"))
                        .size(11.0)
                        .color(Color32::from_gray(0x44)),
                );
            });
        });
}

fn nav_item(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    count: Option<(usize, usize)>,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    let bg = if selected {
        ACCENT.linear_multiply(0.22)
    } else if resp.hovered() {
        ROW_HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, Rounding::same(7.0), bg);
    if selected {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height())),
            Rounding::same(2.0),
            ACCENT,
        );
    }
    let text_col = if selected {
        TEXT
    } else {
        Color32::from_gray(0xbe)
    };
    ui.painter().text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        text_col,
    );
    if let Some((a, t)) = count {
        // keep the count readable on the highlighted (selected) row
        let col = if selected {
            Color32::from_gray(0xd2)
        } else if a == 0 {
            Color32::from_gray(0x44)
        } else {
            GREY
        };
        let label = if a == 0 {
            "off".to_owned()
        } else {
            format!("{a}/{t}")
        };
        ui.painter().text(
            rect.right_center() - egui::vec2(10.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            label,
            egui::FontId::proportional(12.0),
            col,
        );
    }
    resp
}

fn feature_panel(app: &mut App, ui: &mut egui::Ui) {
    let g = app.selected;
    let ct = app.custom_tab;
    let on = app
        .features
        .iter()
        .filter(|f| in_view(f, g, ct))
        .filter(|f| app.effective(f))
        .count();
    let total = app.features.iter().filter(|f| in_view(f, g, ct)).count();
    let all_on = total > 0
        && app
            .features
            .iter()
            .filter(|f| in_view(f, g, ct))
            .all(|f| f.enabled);
    let games_lib = g == Group::Games && app.show_library;

    // The Custom title follows its active tab so it isn't ambiguous.
    let (title, blurb): (&str, &str) = if g == Group::Custom {
        match ct {
            Kind::Action => (
                "Custom Actions",
                "Arbitrary commands and shortcuts HA can trigger.",
            ),
            Kind::Sensor => ("Custom Sensors", "Your own sensors, built from any value."),
        }
    } else {
        (g.name(), g.blurb())
    };

    // ---- title row: name + master toggle, with bulk All (or Scan) on the right ----
    let mut set_all: Option<bool> = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().size(22.0).color(TEXT));
        ui.add_space(GAP);
        match g {
            Group::Custom => match ct {
                Kind::Action => {
                    toggle(ui, &mut app.custom_actions_on);
                }
                Kind::Sensor => {
                    toggle(ui, &mut app.custom_sensors_on);
                }
            },
            _ => {
                let i = g.index();
                // The master is derived from the features (on if any is enabled),
                // so it never drifts from what's persisted. Toggling it is a bulk
                // switch: write the new state through to every feature in the
                // group, rather than keeping a display-only overlay that would be
                // lost on save/reload.
                let derived = app.features.iter().any(|f| f.group == g && f.enabled);
                app.group_on[i] = derived;
                toggle(ui, &mut app.group_on[i]);
                if app.group_on[i] != derived {
                    let on = app.group_on[i];
                    for f in app.features.iter_mut().filter(|f| f.group == g) {
                        f.enabled = on;
                    }
                }
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if games_lib {
                // Games are managed in the library view below (Add game / Scan now);
                // no duplicate control in the group header.
            } else if g == Group::Custom {
                // Custom entities have no per-item toggle; the master gates them
                // all, so show how many are defined instead of an All toggle.
                let n = match ct {
                    Kind::Action => app.custom_commands.len(),
                    Kind::Sensor => app.custom_sensors.len(),
                };
                ui.label(RichText::new(format!("{n} defined")).size(12.0).color(GREY));
            } else {
                let mut all = all_on;
                ui.label(
                    RichText::new(format!("{on}/{total} on"))
                        .size(12.0)
                        .color(GREY),
                );
                ui.add_space(GAP);
                if toggle(ui, &mut all).changed() {
                    set_all = Some(all);
                }
                ui.add_space(GAP);
                ui.label(RichText::new("All").size(12.0).color(TEXT_DIM));
            }
        });
    });
    if let Some(v) = set_all {
        for f in app.features.iter_mut().filter(|f| in_view(f, g, ct)) {
            f.enabled = v;
        }
    }
    ui.add_space(TIGHT);
    ui.label(RichText::new(blurb).size(13.0).color(TEXT_DIM));
    ui.add_space(BLOCK);

    // ---- tabs ----
    if g == Group::Games {
        ui.horizontal(|ui| {
            if ui.selectable_label(!app.show_library, "Features").clicked() {
                app.show_library = false;
            }
            if ui.selectable_label(app.show_library, "Library").clicked() {
                app.show_library = true;
            }
        });
        ui.add_space(GAP);
        if app.show_library {
            library_view(app, ui);
            return;
        }
    } else if g == Group::Custom {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(app.custom_tab == Kind::Action, "Actions")
                .clicked()
            {
                app.custom_tab = Kind::Action;
            }
            if ui
                .selectable_label(app.custom_tab == Kind::Sensor, "Sensors")
                .clicked()
            {
                app.custom_tab = Kind::Sensor;
            }
        });
        ui.add_space(GAP);
    }

    search_row(app, ui);
    ui.add_space(GAP);

    let ct = app.custom_tab;
    let master_on = match g {
        Group::Custom => match ct {
            Kind::Action => app.custom_actions_on,
            Kind::Sensor => app.custom_sensors_on,
        },
        _ => app.group_on[g.index()],
    };
    // Custom entities are user-defined rows edited in place (name, type, and
    // type-specific fields), not static features, so they get their own form
    // view bound directly to the config Vecs.
    if g == Group::Custom {
        custom_view(app, ui, master_on, ct);
        return;
    }

    let allow = app.allow_privileged;
    let needle = app.search.to_lowercase();

    // Display order: alphabetical by name.
    let mut indices: Vec<usize> = app
        .features
        .iter()
        .enumerate()
        .filter(|(_, f)| in_view(f, g, ct))
        .filter(|(_, f)| needle.is_empty() || f.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    indices.sort_by(|&x, &y| app.features[x].name.cmp(app.features[y].name));

    let mut to_remove: Option<usize> = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for &i in &indices {
                let supported = !app.unsupported.contains(&app.features[i].id);
                if feature_row(ui, &mut app.features[i], master_on, allow, false, supported) {
                    to_remove = Some(i);
                }
                ui.add_space(GAP);
            }
        });
    if let Some(i) = to_remove {
        app.features.remove(i);
    }
}

/// Render the editable custom sensors/commands for the active tab, bound to the
/// config Vecs. Custom entities have no per-item enable toggle (the master
/// toggle gates them all), so each row is a definition form plus a remove.
fn custom_view(app: &mut App, ui: &mut egui::Ui, master_on: bool, ct: Kind) {
    let needle = app.search.to_lowercase();
    let mut remove: Option<usize> = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| match ct {
            Kind::Sensor => {
                for (i, s) in app.custom_sensors.iter_mut().enumerate() {
                    if !needle.is_empty() && !s.name.to_lowercase().contains(&needle) {
                        continue;
                    }
                    if custom_sensor_row(ui, s, master_on, i) {
                        remove = Some(i);
                    }
                    ui.add_space(GAP);
                }
                if ui.button("+  Add custom sensor").clicked() {
                    app.custom_sensors.push(CustomSensor {
                        name: String::new(),
                        sensor_type: CustomSensorType::Powershell,
                        interval_seconds: 30,
                        unit: None,
                        icon: None,
                        script: None,
                        process: None,
                        file_path: None,
                        registry_key: None,
                        registry_value: None,
                    });
                }
                if let Some(i) = remove {
                    app.custom_sensors.remove(i);
                }
            }
            Kind::Action => {
                for (i, c) in app.custom_commands.iter_mut().enumerate() {
                    if !needle.is_empty() && !c.name.to_lowercase().contains(&needle) {
                        continue;
                    }
                    if custom_command_row(ui, c, master_on, i) {
                        remove = Some(i);
                    }
                    ui.add_space(GAP);
                }
                if ui.button("+  Add custom action").clicked() {
                    app.custom_commands.push(CustomCommand {
                        name: String::new(),
                        command_type: CustomCommandType::Shell,
                        icon: None,
                        admin: false,
                        script: None,
                        path: None,
                        args: None,
                        command: None,
                    });
                }
                if let Some(i) = remove {
                    app.custom_commands.remove(i);
                }
            }
        });
}

/// One editable string field bound to an `Option<String>` (blank clears it).
fn opt_field(ui: &mut egui::Ui, label: &str, value: &mut Option<String>, width: f32) {
    let mut text = value.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.add_sized(
            [108.0, 18.0],
            egui::Label::new(RichText::new(label).size(12.0).color(GREY)),
        );
        ui.add(egui::TextEdit::singleline(&mut text).desired_width(width));
    });
    *value = if text.trim().is_empty() {
        None
    } else {
        Some(text)
    };
}

/// Editable form for one custom sensor. Returns true if removed.
fn custom_sensor_row(ui: &mut egui::Ui, s: &mut CustomSensor, master_on: bool, idx: usize) -> bool {
    let fill = if master_on { ROW } else { ROW_OFF };
    let mut remove = false;
    egui::Frame::none()
        .fill(fill)
        .rounding(9.0)
        .inner_margin(egui::Margin::symmetric(PAD_X, PAD_Y))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.name)
                        .hint_text("name")
                        .desired_width(190.0),
                );
                ui.add_space(TIGHT);
                badge(ui, "SENSOR", ACCENT);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("remove").size(12.0).color(GREY))
                                .frame(false),
                        )
                        .clicked()
                    {
                        remove = true;
                    }
                });
            });
            ui.add_space(TIGHT);
            ui.horizontal(|ui| {
                ui.add_sized(
                    [108.0, 18.0],
                    egui::Label::new(RichText::new("Type").size(12.0).color(GREY)),
                );
                egui::ComboBox::from_id_salt(("cs_type", idx))
                    .selected_text(sensor_type_label(&s.sensor_type))
                    .show_ui(ui, |ui| {
                        for t in [
                            CustomSensorType::Powershell,
                            CustomSensorType::ProcessExists,
                            CustomSensorType::FileContents,
                            CustomSensorType::Registry,
                        ] {
                            let lbl = sensor_type_label(&t);
                            ui.selectable_value(&mut s.sensor_type, t, lbl);
                        }
                    });
            });
            ui.add_space(TIGHT);
            // Type-specific input(s).
            match s.sensor_type {
                CustomSensorType::Powershell => {
                    opt_field(ui, "Script", &mut s.script, 300.0);
                }
                CustomSensorType::ProcessExists => {
                    opt_field(ui, "Process", &mut s.process, 220.0);
                }
                CustomSensorType::FileContents => {
                    opt_field(ui, "File path", &mut s.file_path, 300.0);
                }
                CustomSensorType::Registry => {
                    opt_field(ui, "Registry key", &mut s.registry_key, 300.0);
                    ui.add_space(TIGHT);
                    opt_field(ui, "Value name", &mut s.registry_value, 220.0);
                }
            }
            ui.add_space(TIGHT);
            ui.horizontal(|ui| {
                ui.add_sized(
                    [108.0, 18.0],
                    egui::Label::new(RichText::new("Poll every").size(12.0).color(GREY)),
                );
                ui.add(
                    egui::DragValue::new(&mut s.interval_seconds)
                        .suffix(" s")
                        .range(1..=86_400),
                );
            });
            ui.add_space(TIGHT);
            opt_field(ui, "Unit", &mut s.unit, 90.0);
        });
    remove
}

/// Editable form for one custom command. Returns true if removed.
fn custom_command_row(
    ui: &mut egui::Ui,
    c: &mut CustomCommand,
    master_on: bool,
    idx: usize,
) -> bool {
    let fill = if master_on { ROW } else { ROW_OFF };
    let mut remove = false;
    egui::Frame::none()
        .fill(fill)
        .rounding(9.0)
        .inner_margin(egui::Margin::symmetric(PAD_X, PAD_Y))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut c.name)
                        .hint_text("name")
                        .desired_width(190.0),
                );
                ui.add_space(TIGHT);
                badge(ui, "ACTION", ORANGE);
                if c.admin {
                    ui.add_space(TIGHT);
                    badge(ui, "ADMIN", RED);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("remove").size(12.0).color(GREY))
                                .frame(false),
                        )
                        .clicked()
                    {
                        remove = true;
                    }
                });
            });
            ui.add_space(TIGHT);
            ui.horizontal(|ui| {
                ui.add_sized(
                    [108.0, 18.0],
                    egui::Label::new(RichText::new("Type").size(12.0).color(GREY)),
                );
                egui::ComboBox::from_id_salt(("cc_type", idx))
                    .selected_text(command_type_label(&c.command_type))
                    .show_ui(ui, |ui| {
                        for t in [
                            CustomCommandType::Shell,
                            CustomCommandType::Powershell,
                            CustomCommandType::Executable,
                        ] {
                            let lbl = command_type_label(&t);
                            ui.selectable_value(&mut c.command_type, t, lbl);
                        }
                    });
            });
            ui.add_space(TIGHT);
            match c.command_type {
                CustomCommandType::Shell => {
                    opt_field(ui, "Command", &mut c.command, 300.0);
                }
                CustomCommandType::Powershell => {
                    opt_field(ui, "Script", &mut c.script, 300.0);
                }
                CustomCommandType::Executable => {
                    opt_field(ui, "Path", &mut c.path, 300.0);
                }
            }
            ui.add_space(TIGHT);
            ui.horizontal(|ui| {
                ui.add_sized(
                    [108.0, 18.0],
                    egui::Label::new(RichText::new("Run as admin").size(12.0).color(GREY)),
                );
                toggle(ui, &mut c.admin);
            });
        });
    remove
}

fn sensor_type_label(t: &CustomSensorType) -> &'static str {
    match t {
        CustomSensorType::Powershell => "PowerShell",
        CustomSensorType::ProcessExists => "Process running",
        CustomSensorType::FileContents => "File contents",
        CustomSensorType::Registry => "Registry value",
    }
}

fn command_type_label(t: &CustomCommandType) -> &'static str {
    match t {
        CustomCommandType::Shell => "Shell command",
        CustomCommandType::Powershell => "PowerShell script",
        CustomCommandType::Executable => "Executable",
    }
}

fn search_row(app: &mut App, ui: &mut egui::Ui) {
    egui::Frame::none()
        .fill(ROW)
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(PAD_X, 7.0))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text("Search")
                    .frame(false)
                    .desired_width(f32::INFINITY),
            );
        });
}

#[allow(clippy::fn_params_excessive_bools)]
fn feature_row(
    ui: &mut egui::Ui,
    f: &mut Feature,
    master_on: bool,
    allow_privileged: bool,
    removable: bool,
    supported: bool,
) -> bool {
    if !supported {
        // Can't run on this session (X11-only feature on Wayland): keep it off.
        f.enabled = false;
    }
    let blocked = f.privileged && !allow_privileged;
    let effective = master_on && f.enabled && !blocked && supported;
    let fill = if effective { ROW } else { ROW_OFF };
    let mut remove_clicked = false;
    egui::Frame::none()
        .fill(fill)
        .rounding(9.0)
        .inner_margin(egui::Margin::symmetric(PAD_X, PAD_Y))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let dot_col = if !effective {
                    GREY
                } else if f.status == Status::Error {
                    RED
                } else {
                    GREEN
                };
                dot(ui, dot_col, 5.0);
                ui.add_space(GAP);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let title_col = if effective { TEXT } else { GREY };
                        ui.label(RichText::new(f.name).strong().size(15.0).color(title_col));
                        ui.add_space(TIGHT);
                        match f.kind {
                            Kind::Sensor => badge(ui, "SENSOR", ACCENT),
                            Kind::Action => badge(ui, "ACTION", ORANGE),
                        }
                        if f.privileged {
                            badge(ui, "ADMIN", RED);
                        }
                        if !f.requires.is_empty() {
                            badge(ui, "SETUP", AMBER);
                        }
                    });
                    ui.label(RichText::new(f.desc).size(12.0).color(TEXT_DIM));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if removable {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("remove").size(12.0).color(GREY))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            remove_clicked = true;
                        }
                        ui.add_space(GAP);
                    }
                    let lbl = if f.expanded { "hide" } else { "details" };
                    if ui
                        .add(
                            egui::Button::new(RichText::new(lbl).size(12.0).color(GREY))
                                .frame(false),
                        )
                        .clicked()
                    {
                        f.expanded = !f.expanded;
                    }
                    ui.add_space(GAP);
                    ui.add_enabled_ui(master_on && !blocked && supported, |ui| {
                        toggle(ui, &mut f.enabled);
                    });
                });
            });

            if !supported {
                ui.add_space(TIGHT);
                kv(
                    ui,
                    "Unavailable",
                    "not supported on this session (needs an X11 desktop, not Wayland)",
                    AMBER,
                    false,
                );
            }

            if blocked {
                ui.add_space(TIGHT);
                ui.label(
                    RichText::new("   locked: turn on Allow privileged commands in General")
                        .color(RED)
                        .size(12.0),
                );
            } else if effective && f.status == Status::Error {
                ui.add_space(TIGHT);
                ui.label(
                    RichText::new("   error: adapter not found")
                        .color(RED)
                        .size(12.0),
                );
            }

            if f.expanded {
                ui.add_space(GAP);
                ui.separator();
                ui.add_space(TIGHT);
                kv(ui, "Reports as", &f.entity, ACCENT, true);
                if !f.requires.is_empty() {
                    kv(ui, "Requires", f.requires, AMBER, false);
                }
                if !f.method.is_empty() {
                    kv(ui, "How", f.method, TEXT_DIM, false);
                }
                if f.privileged {
                    kv(
                        ui,
                        "Security",
                        "elevated or destructive; gated by the Security setting",
                        RED,
                        false,
                    );
                }
                ui.add_space(TIGHT);
                match f.kind {
                    Kind::Sensor => {
                        ui.horizontal(|ui| {
                            if f.interval == 0 {
                                ui.add_sized(
                                    [110.0, 18.0],
                                    egui::Label::new(
                                        RichText::new("Updates").size(12.0).color(GREY),
                                    ),
                                );
                                ui.label(RichText::new("Event-driven").size(12.0).color(TEXT_DIM));
                            } else {
                                ui.add_sized(
                                    [110.0, 18.0],
                                    egui::Label::new(
                                        RichText::new("Poll every").size(12.0).color(GREY),
                                    ),
                                );
                                ui.add(
                                    // Wide range so editing one slider can't clamp a
                                    // legitimately large configured interval (up to 1h).
                                    egui::DragValue::new(&mut f.interval)
                                        .range(1..=3600)
                                        .suffix(" s"),
                                );
                            }
                        });
                        ui.add_space(TIGHT);
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [110.0, 18.0],
                                egui::Label::new(RichText::new("Example").size(12.0).color(GREY)),
                            );
                            ui.label(RichText::new(&f.value).monospace().color(if effective {
                                ACCENT
                            } else {
                                GREY
                            }));
                        });
                    }
                    Kind::Action => {
                        // Built-in action commands are hardcoded (resolved in the
                        // executor), not config-driven, so this is display-only.
                        // User-defined commands live in the Custom Actions tab.
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [110.0, 18.0],
                                egui::Label::new(RichText::new("Command").size(12.0).color(GREY)),
                            );
                            ui.label(RichText::new(&f.value).monospace().color(if effective {
                                ACCENT
                            } else {
                                GREY
                            }));
                        });
                    }
                }
            }
        });
    remove_clicked
}

fn launcher_color(l: Launcher) -> Color32 {
    match l {
        Launcher::Steam => ACCENT,
        Launcher::Epic => Color32::from_gray(0xcc),
        Launcher::Xbox => GREEN,
        Launcher::Gog => PURPLE,
        Launcher::Manual => GREY,
    }
}

fn legend_item(ui: &mut egui::Ui, color: Color32, label: &str) {
    dot(ui, color, 4.0);
    ui.add_space(TIGHT);
    ui.label(RichText::new(label).size(11.0).color(GREY));
    ui.add_space(GAP);
}

fn library_view(app: &mut App, ui: &mut egui::Ui) {
    let exposed = app.library.iter().filter(|g| g.exposed).count();

    ui.horizontal(|ui| {
        if ui.button("+  Add game").clicked() {
            app.library.push(Game {
                name: "New Game".to_owned(),
                process: "game.exe".to_owned(),
                path: String::new(),
                appid: 0,
                launcher: Launcher::Manual,
                status: GameStatus::Installed,
                exposed: false,
                game_id: String::new(),
            });
        }
        let _ = ui.button("Scan now");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} games · {exposed} exposed", app.library.len()))
                    .size(12.0)
                    .color(TEXT_DIM),
            );
        });
    });
    ui.add_space(GAP);

    // legend / column hint
    ui.horizontal(|ui| {
        legend_item(ui, GREEN, "Running");
        legend_item(ui, ACCENT, "Downloading");
        legend_item(ui, ORANGE, "Update");
        legend_item(ui, TEXT_DIM, "Installed");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("Expose to HA").size(11.0).color(GREY));
        });
    });
    ui.add_space(GAP);

    let mut remove: Option<usize> = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for (i, game) in app.library.iter_mut().enumerate() {
                let (stext, scol) = match game.status {
                    GameStatus::Running => ("Running".to_owned(), GREEN),
                    GameStatus::Downloading(p) => (format!("Downloading {p}%"), ACCENT),
                    GameStatus::UpdatePending => ("Update".to_owned(), ORANGE),
                    GameStatus::Installed => ("Installed".to_owned(), TEXT_DIM),
                };
                egui::Frame::none()
                    .fill(ROW)
                    .rounding(9.0)
                    .inner_margin(egui::Margin::symmetric(PAD_X, PAD_Y))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            dot(ui, scol, 4.5);
                            ui.add_space(GAP);
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut game.name)
                                            .desired_width(190.0),
                                    );
                                    ui.add_space(TIGHT);
                                    badge(ui, game.launcher.tag(), launcher_color(game.launcher));
                                    ui.add_space(TIGHT);
                                    let appid = if game.appid == 0 {
                                        "no appid".to_owned()
                                    } else {
                                        format!("appid {}", game.appid)
                                    };
                                    ui.label(RichText::new(appid).size(11.0).color(GREY));
                                });
                                ui.add_space(TIGHT);
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [46.0, 18.0],
                                        egui::Label::new(
                                            RichText::new("process").size(11.0).color(GREY),
                                        ),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut game.process)
                                            .desired_width(190.0)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                });
                                ui.add_space(TIGHT);
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [46.0, 18.0],
                                        egui::Label::new(
                                            RichText::new("path").size(11.0).color(GREY),
                                        ),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut game.path)
                                            .desired_width(280.0)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                    if ui.button("Browse").clicked()
                                        && let Some(p) = rfd::FileDialog::new().pick_folder()
                                    {
                                        game.path = p.display().to_string();
                                    }
                                });
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("remove").size(12.0).color(GREY),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        remove = Some(i);
                                    }
                                    ui.add_space(GAP);
                                    toggle(ui, &mut game.exposed);
                                    ui.add_space(BLOCK);
                                    ui.label(RichText::new(stext).size(12.0).color(scol));
                                },
                            );
                        });
                    });
                ui.add_space(GAP);
            }
        });
    if let Some(i) = remove {
        app.library.remove(i);
    }
}

fn general_panel(app: &mut App, ui: &mut egui::Ui) {
    ui.label(RichText::new("General").strong().size(22.0).color(TEXT));
    ui.add_space(TIGHT);
    ui.label(
        RichText::new(Group::General.blurb())
            .size(13.0)
            .color(TEXT_DIM),
    );
    ui.add_space(BLOCK);

    let priv_count = app.features.iter().filter(|f| f.privileged).count();

    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        section(ui, "Connection", |ui| {
            // pin the section width so it stays the same length across transports
            ui.set_width(430.0);
            labeled(ui, "Device name", |ui| {
                ui.add(egui::TextEdit::singleline(&mut app.device).desired_width(240.0));
            });
            labeled(ui, "Integration", |ui| {
                ui.selectable_value(&mut app.transport, Transport::Mqtt, "MQTT");
                // Native (HACS) transport isn't implemented yet (Config has no
                // transport/token field). Show it disabled + labelled unsupported
                // rather than let a user pick it + paste a token Save would discard.
                ui.add_enabled_ui(false, |ui| {
                    ui.selectable_value(
                        &mut app.transport,
                        Transport::Native,
                        "Native (HACS) (unsupported)",
                    )
                    .on_disabled_hover_text("Native HACS integration is not supported yet");
                });
            });
            match app.transport {
                Transport::Mqtt => {
                    labeled(ui, "Broker", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut app.mqtt_host).desired_width(190.0));
                        ui.label(":");
                        ui.add(egui::TextEdit::singleline(&mut app.mqtt_port).desired_width(56.0));
                    });
                    labeled(ui, "Username", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut app.mqtt_user).desired_width(220.0));
                    });
                    labeled(ui, "Password", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut app.mqtt_pass).desired_width(220.0).password(!app.show_secrets));
                        if ui.small_button(if app.show_secrets { "hide" } else { "show" }).clicked() {
                            app.show_secrets = !app.show_secrets;
                        }
                    });
                }
                Transport::Native => {
                    labeled(ui, "Token", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut app.ha_token).desired_width(220.0).password(!app.show_secrets).hint_text("long-lived access token"));
                        if ui.small_button(if app.show_secrets { "hide" } else { "show" }).clicked() {
                            app.show_secrets = !app.show_secrets;
                        }
                    });
                    labeled(ui, "Discovery", |ui| {
                        ui.label(RichText::new("Automatic over mDNS, no broker needed").size(13.0).color(TEXT_DIM));
                    });
                }
            }
            ui.add_space(GAP);
            if let Some(err) = &app.load_error {
                ui.label(
                    RichText::new(format!("Config failed to load: {err}"))
                        .color(RED)
                        .size(13.0),
                );
                ui.label(
                    RichText::new(
                        "Saving is disabled to avoid overwriting your existing config. Fix the file or credential and reopen.",
                    )
                    .color(TEXT_DIM)
                    .size(12.0),
                );
                ui.add_space(GAP);
            }
            if let Some(err) = &app.save_error {
                ui.label(RichText::new(format!("Could not save: {err}")).color(RED).size(13.0));
                ui.add_space(GAP);
            }
            let can_save = app.load_error.is_none();
            ui.horizontal(|ui| {
                ui.add_enabled_ui(can_save, |ui| {
                    if ui.button("Save").clicked() {
                        // Saving writes the config; it does NOT contact the broker,
                        // so it must not claim "connected" (that's Test connection).
                        match app.save() {
                            Ok(()) => {
                                app.saved = true;
                                app.save_error = None;
                            }
                            Err(e) => {
                                app.saved = false;
                                app.save_error = Some(format!("{e:#}"));
                            }
                        }
                    }
                });
                if app.saved && app.save_error.is_none() {
                    ui.add_space(GAP);
                    ui.label(RichText::new("Saved").color(GREEN).size(13.0));
                }
                ui.add_space(GAP);
                // Live status from the MQTT subscriber: is the broker reachable and is
                // the agent actually connected + publishing.
                let live = app.live.snapshot();
                let (txt, col) = if !live.attempted {
                    ("connecting...", GREY)
                } else if !live.broker_connected {
                    ("broker unreachable", RED)
                } else if live.agent_online == Some(false) {
                    ("agent offline", ORANGE)
                } else if live.agent_online == Some(true) {
                    ("connected", GREEN)
                } else {
                    ("broker up, agent unknown", AMBER)
                };
                dot(ui, col, 4.5);
                ui.label(RichText::new(txt).color(col).size(13.0));
            });
        });
        ui.add_space(BLOCK);

        section(ui, "Security", |ui| {
            switch_row(
                ui,
                "Allow privileged commands",
                &format!("{priv_count} actions need admin or are destructive (shutdown, restart, custom command). Off blocks them all."),
                &mut app.allow_privileged,
            );
            switch_row(
                ui,
                "Allow launching any game",
                "On: launch commands can start any Steam/Epic title. Off: only games in your list.",
                &mut app.allow_global_launch,
            );
            switch_row(
                ui,
                "Allow closing any process",
                "On: close/kill commands can target any process by name. Off (default): only your configured games.",
                &mut app.allow_global_close,
            );
        });
        ui.add_space(BLOCK);

        section(ui, "Behavior", |ui| {
            switch_row(ui, "Beta updates", "Get pre-release builds.", &mut app.beta_updates);
            switch_row(
                ui,
                "Show tray icon",
                "System tray icon with Open Settings / Quit (Windows). Toggles live.",
                &mut app.show_tray_icon,
            );
        });
        ui.add_space(BLOCK);

        section(ui, "About", |ui| {
            ui.horizontal(|ui| {
                let _ = ui.button("View logs");
                let _ = ui.button("Open config folder");
                let _ = ui.button("Check for updates");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(concat!("pc-bridge v", env!("CARGO_PKG_VERSION")))
                            .size(12.0)
                            .color(GREY),
                    );
                });
            });
        });
    });
}

fn switch_row(ui: &mut egui::Ui, title: &str, desc: &str, on: &mut bool) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(14.0).color(TEXT));
            ui.label(RichText::new(desc).size(12.0).color(TEXT_DIM));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            toggle(ui, on);
        });
    });
}
