//! OneDrive status flyout — a small window opened from the tray icon.
//!
//! Reads everything over the daemon's D-Bus interface; owns no sync logic.

mod daemon;
mod ui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([380.0, 470.0])
            .with_min_inner_size([340.0, 380.0])
            .with_resizable(false)
            .with_title("OneDrive"),
        ..Default::default()
    };
    eframe::run_native(
        "OneDrive",
        options,
        Box::new(|cc| Ok(Box::new(ui::FlyoutApp::new(cc)))),
    )
}
