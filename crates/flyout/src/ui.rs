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

/// How long the window says "Starting…" after asking systemd to launch the
/// daemon, before admitting it did not come up.
const STARTUP_GRACE: Duration = Duration::from_secs(15);

enum View {
    Status,
    /// Device-code sign-in: message, user code, verification URL.
    SignIn {
        user_code: String,
        url: String,
        copied: bool,
    },
}

pub struct FlyoutApp {
    client: DaemonClient,
    snap: Snapshot,
    last_fetch: Instant,
    view: View,
    /// Set once the compositor has granted focus — see the dismissal logic in
    /// `update`.
    has_been_focused: bool,
    /// True when opened from the tray icon (`--flyout`), where clicking
    /// elsewhere should dismiss the window. Launched from the application menu
    /// it is an ordinary window and must stay put until closed.
    dismiss_on_blur: bool,
    /// Set when the daemon was not running and we asked systemd to start it,
    /// so the window says "Starting…" for a grace period instead of the more
    /// alarming "Daemon not running".
    starting_since: Option<Instant>,
}

impl FlyoutApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.style_mut(|style| {
            style.spacing.item_spacing.y = 8.0;
        });
        let client = DaemonClient::connect();
        // Opening the app is how most people will start syncing, so bring the
        // service up rather than telling them to run systemctl.
        let starting = if client.reachable() {
            false
        } else {
            client.start_daemon()
        };
        let snap = client.fetch();
        let mut app = Self {
            client,
            snap,
            last_fetch: Instant::now(),
            view: View::Status,
            has_been_focused: false,
            starting_since: starting.then(Instant::now),
            dismiss_on_blur: std::env::args().any(|a| a == "--flyout"),
        };
        // `onedrive-flyout --signin` (used by the tray's "Sign in" action)
        // jumps straight into the sign-in flow.
        if std::env::args().any(|a| a == "--signin") {
            app.begin_sign_in();
        }
        app
    }

    fn begin_sign_in(&mut self) {
        if let Some((_msg, user_code, url)) = self.client.start_auth() {
            self.view = View::SignIn {
                user_code,
                url,
                copied: false,
            };
        }
    }

    fn headline(&self) -> (Color32, String) {
        let s = &self.snap;
        if !s.reachable {
            match self.starting_since {
                Some(t) if t.elapsed() < STARTUP_GRACE => (ACCENT, "Starting…".into()),
                _ => (MUTED, "Daemon not running".into()),
            }
        } else if s.paused {
            (WARN, "Paused".into())
        } else if s.needs_auth {
            (WARN, "Sign in required".into())
        } else if s.errors > 0 {
            (BAD, format!("Needs attention · {} errors", s.errors))
        } else if !s.progress.is_empty() {
            // The engine's own words beat a counter — during the initial delta
            // there is nothing to count yet, and silence reads as "stuck".
            (ACCENT, s.progress.clone())
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

        if let View::SignIn {
            user_code,
            url,
            copied,
        } = &mut self.view
        {
            let user_code = user_code.clone();
            let url = url.clone();
            let was_copied = *copied;
            // Auth finished (daemon resumed): return to the status view.
            if !self.snap.needs_auth && self.snap.reachable && !self.snap.paused {
                self.view = View::Status;
                return;
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("☁").size(34.0).color(ACCENT));
                    ui.label(RichText::new("Sign in to OneDrive").strong().size(17.0));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Enter this code on the Microsoft sign-in page:")
                            .color(MUTED)
                            .size(12.5),
                    );
                    ui.add_space(8.0);
                    egui::Frame::none()
                        .fill(ACCENT.gamma_multiply(0.12))
                        .rounding(Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(18.0, 10.0))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(&user_code)
                                    .monospace()
                                    .size(24.0)
                                    .strong()
                                    .color(ACCENT),
                            );
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(ui.available_width() / 2.0 - 110.0);
                        let copy_label = if was_copied {
                            "Copied ✓"
                        } else {
                            "Copy code"
                        };
                        if ui.button(copy_label).clicked() {
                            ui.output_mut(|o| o.copied_text = user_code.clone());
                            if let View::SignIn { copied, .. } = &mut self.view {
                                *copied = true;
                            }
                        }
                        if ui.button("Open sign-in page").clicked() {
                            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                        }
                    });
                    ui.add_space(14.0);
                    ui.spinner();
                    ui.label(
                        RichText::new(
                            "Waiting for you to finish signing in…
Sync resumes automatically.",
                        )
                        .color(MUTED)
                        .size(11.5),
                    );
                });
            });
            return;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ── Header ────────────────────────────────────────────────
            let (color, text) = self.headline();
            let mut sign_in_clicked = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("☁").size(24.0).color(ACCENT));
                ui.vertical(|ui| {
                    ui.label(RichText::new("OneDrive").strong().size(16.0));
                    ui.label(RichText::new(text).color(color).size(12.5));
                });
                if self.snap.needs_auth {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Sign in…").clicked() {
                            sign_in_clicked = true;
                        }
                    });
                }
            });
            if sign_in_clicked {
                self.begin_sign_in();
            }

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

        // Flyout behavior: dismiss when the user clicks elsewhere. Under Wayland
        // the compositor reports the window as unfocused for the first frames,
        // before it has granted focus — closing on that would make the window
        // appear and vanish immediately. Only arm the dismissal once focus has
        // actually been held at least once.
        if !self.dismiss_on_blur {
            return;
        }
        if ctx.input(|i| i.viewport().focused == Some(true)) {
            self.has_been_focused = true;
        } else if self.has_been_focused && ctx.input(|i| i.viewport().focused == Some(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
