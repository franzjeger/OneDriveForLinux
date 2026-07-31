//! The flyout window UI — visual spec from the design concept:
//! header (status), storage meter, recent activity with state chips,
//! and a three-button action row.

use crate::daemon::{human_bytes, relative_time, DaemonClient, Snapshot};
use eframe::egui::{self, Color32, RichText, Rounding};
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(0x5A, 0xA2, 0xDD);
const GOOD: Color32 = Color32::from_rgb(0x57, 0xB1, 0x83);
const WARN: Color32 = Color32::from_rgb(0xCF, 0xA0, 0x4A);
const BAD: Color32 = Color32::from_rgb(0xD0, 0x71, 0x6A);
const MUTED: Color32 = Color32::from_rgb(0x8E, 0x9C, 0xAC);

const REFRESH: Duration = Duration::from_secs(2);

pub struct FlyoutApp {
    client: DaemonClient,
    snap: Snapshot,
    last_fetch: Instant,
}

impl FlyoutApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.style_mut(|style| {
            style.spacing.item_spacing.y = 8.0;
        });
        let client = DaemonClient::connect();
        let snap = client.fetch();
        Self {
            client,
            snap,
            last_fetch: Instant::now(),
        }
    }

    fn headline(&self) -> (Color32, String) {
        let s = &self.snap;
        if !s.reachable {
            (MUTED, "Daemon not running".into())
        } else if s.paused {
            (WARN, "Paused".into())
        } else if s.errors > 0 {
            (BAD, format!("Needs attention · {} errors", s.errors))
        } else if s.syncing > 0 {
            (ACCENT, format!("Syncing {} items…", s.syncing))
        } else {
            (GOOD, "Up to date".into())
        }
    }
}

fn state_chip(state: &str) -> (Color32, &'static str) {
    match state {
        "Synced" | "Partially synced" => (GOOD, "Synced"),
        "Syncing" => (ACCENT, "Syncing"),
        "Pinned" => (WARN, "Pinned"),
        "Cloud only" => (MUTED, "Cloud only"),
        "Local only" => (ACCENT, "Uploading"),
        "Conflict" => (BAD, "Conflict"),
        s if s.starts_with("Error") => (BAD, "Error"),
        _ => (MUTED, "—"),
    }
}

impl eframe::App for FlyoutApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_fetch.elapsed() >= REFRESH {
            self.snap = self.client.fetch();
            self.last_fetch = Instant::now();
        }
        ctx.request_repaint_after(REFRESH);

        egui::CentralPanel::default().show(ctx, |ui| {
            // ── Header ────────────────────────────────────────────────
            let (color, text) = self.headline();
            ui.horizontal(|ui| {
                ui.label(RichText::new("☁").size(24.0).color(ACCENT));
                ui.vertical(|ui| {
                    ui.label(RichText::new("OneDrive").strong().size(16.0));
                    ui.label(RichText::new(text).color(color).size(12.5));
                });
            });

            // ── Storage ───────────────────────────────────────────────
            let s = &self.snap;
            if s.quota_total > 0 {
                let frac = (s.quota_used as f32 / s.quota_total as f32).clamp(0.0, 1.0);
                let bar = egui::ProgressBar::new(frac)
                    .desired_height(6.0)
                    .rounding(Rounding::same(3.0))
                    .fill(ACCENT);
                ui.add(bar);
                ui.label(
                    RichText::new(format!(
                        "{} of {} used · {} items tracked",
                        human_bytes(s.quota_used),
                        human_bytes(s.quota_total),
                        s.total_items
                    ))
                    .color(MUTED)
                    .size(11.5),
                );
            }

            ui.separator();

            // ── Recent activity ───────────────────────────────────────
            ui.label(
                RichText::new("RECENT ACTIVITY")
                    .size(10.5)
                    .color(MUTED)
                    .strong(),
            );
            egui::ScrollArea::vertical().show(ui, |ui| {
                if s.recent.is_empty() {
                    ui.label(RichText::new("No activity yet.").color(MUTED));
                }
                for (name, parent, state, ts) in s.recent.clone() {
                    let (chip_color, chip_text) = state_chip(&state);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.style_mut().spacing.item_spacing.y = 1.0;
                            ui.label(RichText::new(&name).strong().size(13.0));
                            ui.label(
                                RichText::new(format!("{parent} · {}", relative_time(ts)))
                                    .color(MUTED)
                                    .size(11.0),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            egui::Frame::none()
                                .fill(chip_color.gamma_multiply(0.16))
                                .rounding(Rounding::same(8.0))
                                .inner_margin(egui::Margin::symmetric(8.0, 2.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(chip_text)
                                            .color(chip_color)
                                            .size(11.0)
                                            .strong(),
                                    );
                                });
                        });
                    });
                }
            });

            // ── Actions ───────────────────────────────────────────────
            egui::TopBottomPanel::bottom("actions")
                .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(0.0, 8.0)))
                .show_inside(ui, |ui| {
                    ui.separator();
                    ui.columns(3, |cols| {
                        if cols[0].button("Open folder").clicked() {
                            let dir = dirs::home_dir().unwrap_or_default().join("OneDrive");
                            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                        }
                        let pause_label = if self.snap.paused {
                            "Resume sync"
                        } else {
                            "Pause sync"
                        };
                        if cols[1].button(pause_label).clicked() {
                            self.client.set_paused(!self.snap.paused);
                            self.snap = self.client.fetch();
                        }
                        if cols[2].button("Settings").clicked() {
                            let cfg = dirs::config_dir()
                                .unwrap_or_default()
                                .join("onedrive-linux/config.toml");
                            let _ = std::process::Command::new("xdg-open").arg(cfg).spawn();
                        }
                    });
                });
        });

        // Flyout behavior: dismiss when the user clicks elsewhere.
        if ctx.input(|i| i.viewport().focused == Some(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
