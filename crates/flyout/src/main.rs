//! OneDrive status flyout — a small window opened from the tray icon.
//!
//! Reads everything over the daemon's D-Bus interface; owns no sync logic.

mod daemon;
mod settings;
mod ui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([380.0, 470.0])
            .with_min_inner_size([340.0, 380.0])
            // The settings view is taller than the status view; let people
            // size the window rather than scroll a cramped panel.
            .with_resizable(true)
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

#[cfg(test)]
mod tests {
    /// Guards the `default_fonts` feature on eframe. Without it egui embeds no
    /// font data and every label in the window renders as nothing, while bars,
    /// separators and button frames still draw — the window looks blank rather
    /// than broken, which is very easy to misread as a daemon problem.
    #[test]
    fn egui_has_embedded_fonts() {
        let fonts = eframe::egui::FontDefinitions::default();
        assert!(
            !fonts.font_data.is_empty(),
            "egui was built without embedded fonts — add the `default_fonts` \
             feature to the eframe dependency"
        );
    }
}
