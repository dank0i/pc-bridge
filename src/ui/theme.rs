//! Palette, global style, and the small custom widgets (toggle, status dot,
//! kind badge). Kept separate so the look is defined in one place.

use eframe::egui;
use egui::{Color32, RichText, Rounding, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x12, 0x13, 0x17);
pub const PANEL: Color32 = Color32::from_rgb(0x18, 0x1a, 0x20);
pub const ROW: Color32 = Color32::from_rgb(0x1e, 0x21, 0x29);
pub const ROW_HOVER: Color32 = Color32::from_rgb(0x25, 0x29, 0x33);
pub const ACCENT: Color32 = Color32::from_rgb(0x4c, 0x8b, 0xf5);
pub const GREEN: Color32 = Color32::from_rgb(0x3d, 0xc9, 0x7a);
pub const RED: Color32 = Color32::from_rgb(0xe0, 0x5a, 0x5a);
pub const ORANGE: Color32 = Color32::from_rgb(0xe0, 0x9b, 0x3e);
pub const GREY: Color32 = Color32::from_gray(0x52);
pub const TEXT_DIM: Color32 = Color32::from_gray(0x9a);
pub const TEXT: Color32 = Color32::from_gray(0xe8);
pub const AMBER: Color32 = Color32::from_rgb(0xd9, 0xbf, 0x4a);
pub const PURPLE: Color32 = Color32::from_rgb(0x9b, 0x59, 0xd6);
pub const ROW_OFF: Color32 = Color32::from_rgb(0x16, 0x18, 0x1d);

// Uniform spacing scale. Only these three vertical gaps are used anywhere.
pub const TIGHT: f32 = 4.0;
pub const GAP: f32 = 8.0;
pub const BLOCK: f32 = 14.0;
// Uniform frame padding.
pub const PAD_X: f32 = 14.0;
pub const PAD_Y: f32 = 11.0;

pub fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(0x0e, 0x0f, 0x12);
    v.faint_bg_color = ROW;
    v.selection.bg_fill = ACCENT.linear_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.hovered.bg_fill = ROW_HOVER;
    v.widgets.inactive.bg_fill = ROW;
    v.widgets.active.bg_fill = ACCENT;
    v.widgets.inactive.rounding = Rounding::same(6.0);
    v.widgets.hovered.rounding = Rounding::same(6.0);
    v.widgets.active.rounding = Rounding::same(6.0);
    v.hyperlink_color = ACCENT;
    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}

pub fn dot(ui: &mut egui::Ui, color: Color32, r: f32) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(r * 2.0 + 2.0, r * 2.0 + 2.0),
        egui::Sense::hover(),
    );
    ui.painter().circle_filled(rect.center(), r, color);
}

pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::none()
        .fill(color.linear_multiply(0.16))
        .rounding(4.0)
        .inner_margin(egui::Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(10.0).color(color).strong());
        });
}

pub fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(38.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let bg = lerp_color(Color32::from_gray(0x42), GREEN, t);
    let radius = rect.height() / 2.0;
    ui.painter().rect_filled(rect, Rounding::same(radius), bg);
    let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    ui.painter().circle(
        egui::pos2(cx, rect.center().y),
        radius * 0.72,
        Color32::WHITE,
        Stroke::NONE,
    );
    resp
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.label(
        RichText::new(title.to_uppercase())
            .size(11.0)
            .color(GREY)
            .strong(),
    );
    ui.add_space(4.0);
    egui::Frame::none()
        .fill(ROW)
        .rounding(9.0)
        .inner_margin(egui::Margin::symmetric(PAD_X, PAD_Y))
        .show(ui, |ui| body(ui));
}

/// Key/value note row used in the expanded feature details.
pub fn kv(ui: &mut egui::Ui, key: &str, value: &str, color: Color32, mono: bool) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [110.0, 16.0],
            egui::Label::new(RichText::new(key).size(12.0).color(GREY)),
        );
        let mut t = RichText::new(value).size(12.0).color(color);
        if mono {
            t = t.monospace();
        }
        // Wrap long values (e.g. the HWiNFO sensor list) instead of running off-screen.
        ui.add(egui::Label::new(t).wrap());
    });
}

pub fn labeled(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [120.0, 20.0],
            egui::Label::new(RichText::new(label).size(13.0)),
        );
        body(ui);
    });
}
