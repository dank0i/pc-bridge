//! Native settings UI (egui). Launched via `pc-bridge --ui`; the headless agent
//! never initializes any of this. Stage 1 renders the approved design backed by
//! a self-contained view-model; later stages bind it to the real Config and the
//! running agent.

mod app;
mod live;
mod model;
mod theme;

use eframe::egui;

/// Open the settings window. Blocks until the window is closed.
pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([840.0, 560.0])
            .with_title("pc-bridge"),
        ..Default::default()
    };
    eframe::run_native(
        "pc-bridge",
        options,
        Box::new(|cc| {
            theme::setup_style(&cc.egui_ctx);
            Ok(Box::new(app::App::new()))
        }),
    )
    .map_err(|e| anyhow::anyhow!("settings UI failed: {e}"))?;
    Ok(())
}
