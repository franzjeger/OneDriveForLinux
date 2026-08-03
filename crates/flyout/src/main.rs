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
            .with_title("OneDrive")
            // Must match the basename of onedrive-linux.desktop, or Wayland
            // compositors show the window with a generic icon and refuse to
            // group it under the launcher entry.
            .with_app_id("onedrive-linux"),
        ..Default::default()
    };
    eframe::run_native(
        "OneDrive",
        options,
        Box::new(|cc| Ok(Box::new(ui::FlyoutApp::new(cc)))),
    )
}
