//! App state and all views. A group/subsection master toggle (next to the name)
//! gates features while keeping each one's individual last-known state; a
//! separate bulk "All" toggle sets the individual states. Uniform spacing via
//! the theme scale. No em-dashes anywhere.

#![allow(clippy::too_many_lines)]

use eframe::egui;
use egui::{Color32, RichText, Rounding};

use super::model::{
    Feature, Game, GameStatus, Group, Kind, Launcher, Status, Transport, library, registry,
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
    connected: bool,
    tray_enabled: bool,
    autostart: bool,
    beta_updates: bool,
    allow_privileged: bool,
    confirm_destructive: bool,
    selected: Group,
    search: String,
    show_library: bool,
    custom_tab: Kind,
    group_on: [bool; 8],
    custom_actions_on: bool,
    custom_sensors_on: bool,
    features: Vec<Feature>,
    library: Vec<Game>,
}

impl App {
    pub fn new() -> Self {
        Self {
            device: "dank0i-pc".to_owned(),
            transport: Transport::Mqtt,
            mqtt_host: "homeassistant.local".to_owned(),
            mqtt_port: "1883".to_owned(),
            mqtt_user: "pc-bridge".to_owned(),
            mqtt_pass: String::new(),
            ha_token: String::new(),
            show_secrets: false,
            connected: true,
            tray_enabled: true,
            autostart: true,
            beta_updates: false,
            allow_privileged: true,
            confirm_destructive: true,
            selected: Group::Games,
            search: String::new(),
            show_library: false,
            custom_tab: Kind::Action,
            group_on: [true; 8],
            custom_actions_on: true,
            custom_sensors_on: true,
            features: registry(),
            library: library(),
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

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        top_bar(self, ctx);
        side_rail(self, ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(18.0, 14.0)),
            )
            .show(ctx, |ui| {
                if self.selected == Group::General {
                    general_panel(self, ui);
                } else {
                    feature_panel(self, ui);
                }
            });
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (txt, col) = if app.connected {
                        ("Connected", GREEN)
                    } else {
                        ("Disconnected", RED)
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
                    RichText::new("v2.3.1  ·  prototype")
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
        ui.painter().text(
            rect.right_center() - egui::vec2(10.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{a}/{t}"),
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
                toggle(ui, &mut app.group_on[i]);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if games_lib {
                let _ = ui.button("Scan now");
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
    let allow = app.allow_privileged;
    let needle = app.search.to_lowercase();
    let removable = g == Group::Custom;

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
                if feature_row(ui, &mut app.features[i], master_on, allow, removable) {
                    to_remove = Some(i);
                }
                ui.add_space(GAP);
            }
            if g == Group::Custom {
                let label = match ct {
                    Kind::Action => "+  Add custom action",
                    Kind::Sensor => "+  Add custom sensor",
                };
                if ui.button(label).clicked() {
                    app.features.push(Feature {
                        id: "custom_new",
                        name: "New",
                        desc: "A custom item you defined.",
                        group: Group::Custom,
                        kind: ct,
                        privileged: false,
                        enabled: false,
                        status: Status::Running,
                        value: String::new(),
                        interval: if ct == Kind::Sensor { 5 } else { 0 },
                        entity: "sensor.dank0i_pc_custom_new",
                        requires: "",
                        method: "User-defined",
                        expanded: true,
                    });
                }
            }
        });
    if let Some(i) = to_remove {
        app.features.remove(i);
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

fn feature_row(
    ui: &mut egui::Ui,
    f: &mut Feature,
    master_on: bool,
    allow_privileged: bool,
    removable: bool,
) -> bool {
    let blocked = f.privileged && !allow_privileged;
    let effective = master_on && f.enabled && !blocked;
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
                    ui.add_enabled_ui(master_on && !blocked, |ui| {
                        toggle(ui, &mut f.enabled);
                    });
                });
            });

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
                kv(ui, "Reports as", f.entity, ACCENT, true);
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
                                    egui::DragValue::new(&mut f.interval)
                                        .range(1..=120)
                                        .suffix(" s"),
                                );
                            }
                        });
                        ui.add_space(TIGHT);
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [110.0, 18.0],
                                egui::Label::new(
                                    RichText::new("Now reporting").size(12.0).color(GREY),
                                ),
                            );
                            ui.label(RichText::new(&f.value).monospace().color(if effective {
                                ACCENT
                            } else {
                                GREY
                            }));
                        });
                    }
                    Kind::Action => {
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [110.0, 18.0],
                                egui::Label::new(RichText::new("Command").size(12.0).color(GREY)),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut f.value)
                                    .desired_width(300.0)
                                    .hint_text("command to run"),
                            );
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
    let dl = app
        .library
        .iter()
        .filter(|g| matches!(g.status, GameStatus::Downloading(_)))
        .count();
    let upd = app
        .library
        .iter()
        .filter(|g| g.status == GameStatus::UpdatePending)
        .count();
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
            });
        }
        let _ = ui.button("Scan now");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!(
                    "{} games · {exposed} exposed · {dl} downloading · {upd} updates",
                    app.library.len()
                ))
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
                ui.selectable_value(&mut app.transport, Transport::Native, "Native (HACS)");
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
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    app.connected = true;
                }
                if ui.button("Test connection").clicked() {
                    app.connected = true;
                }
                ui.add_space(GAP);
                let (txt, col) = if app.connected {
                    ("connected", GREEN)
                } else {
                    ("not connected", RED)
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
            ui.add_space(GAP);
            switch_row(ui, "Confirm destructive actions", "Require a confirm in HA before shutdown, restart, or log off.", &mut app.confirm_destructive);
        });
        ui.add_space(BLOCK);

        section(ui, "Behavior", |ui| {
            switch_row(ui, "Show tray icon", "Off keeps the agent fully headless; relaunch the app to open this window.", &mut app.tray_enabled);
            ui.add_space(GAP);
            switch_row(ui, "Start with Windows", "Launch the agent automatically at login.", &mut app.autostart);
            ui.add_space(GAP);
            switch_row(ui, "Beta updates", "Get pre-release builds.", &mut app.beta_updates);
        });
        ui.add_space(BLOCK);

        section(ui, "About", |ui| {
            ui.horizontal(|ui| {
                let _ = ui.button("View logs");
                let _ = ui.button("Open config folder");
                let _ = ui.button("Check for updates");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("pc-bridge v2.3.1").size(12.0).color(GREY));
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
