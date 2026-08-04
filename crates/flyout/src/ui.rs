//! The flyout window UI — visual spec from the design concept:
//! header (status), storage meter, recent activity with state chips,
//! and a three-button action row.

use crate::daemon::{human_bytes, relative_time, DaemonClient, Snapshot};
use crate::settings::{self, Settings, AUTH_METHODS};
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

/// State of the settings view. Boxed inside `View` because it is far larger
/// than the other variants, and an enum is sized by its biggest one.
struct SettingsView {
    draft: Settings,
    /// What the file held when the view opened — used to detect edits.
    original: Settings,
    /// Top-level folder names offered as checkboxes, read once on open.
    folders: Vec<String>,
    /// Result banner shown after a save attempt.
    status: Option<Result<String, String>>,
}

enum View {
    Status,
    /// Editing config.toml through real controls rather than a text editor.
    Settings(Box<SettingsView>),
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
    /// Bytes freed by the last "Free up space", and when. Shown briefly so the
    /// click has a visible result — freeing nothing looks identical to the
    /// button doing nothing at all.
    freed_notice: Option<(u64, Instant)>,
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
            freed_notice: None,
            starting_since: starting.then(Instant::now),
            dismiss_on_blur: std::env::args().any(|a| a == "--flyout"),
        };
        // `onedrive-flyout --signin` (used by the tray's "Sign in" action)
        // jumps straight into the sign-in flow.
        if std::env::args().any(|a| a == "--signin") {
            app.begin_sign_in();
        } else if std::env::args().any(|a| a == "--settings") {
            app.open_settings();
        }
        app
    }

    fn open_settings(&mut self) {
        let loaded = settings::load();
        let (draft, status) = match loaded {
            Ok(s) => (s, None),
            // A config we cannot parse must not be silently replaced with
            // defaults — that would throw away the user's client_id.
            Err(e) => (
                Settings::default(),
                Some(Err(format!("Could not read the config file: {e}"))),
            ),
        };
        self.view = View::Settings(Box::new(SettingsView {
            folders: self.client.top_level_folders(),
            original: draft.clone(),
            draft,
            status,
        }));
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
        } else if s.pending_uploads > 0 {
            // Outranks the progress line: unsent edits are the one thing the
            // user might act on.
            (
                WARN,
                format!("{} file(s) waiting to upload", s.pending_uploads),
            )
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

        if let View::Settings(view) = &mut self.view {
            let SettingsView {
                draft,
                original,
                folders,
                status,
            } = view.as_mut();
            let mut close = false;
            let mut save = false;
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("‹ Back").clicked() {
                        close = true;
                    }
                    ui.label(RichText::new("Settings").strong().size(16.0));
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 46.0)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("SYNC FOLDER")
                                .size(10.5)
                                .color(MUTED)
                                .strong(),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut draft.sync_dir)
                                .desired_width(f32::INFINITY),
                        );
                        ui.label(
                            RichText::new("Where your OneDrive files appear on this computer.")
                                .color(MUTED)
                                .size(11.0),
                        );
                        ui.add_space(6.0);

                        ui.checkbox(&mut draft.on_demand, "Files On-Demand");
                        ui.label(
                            RichText::new(
                                "Show every file without downloading it. Files download when \
                                 you open them. Turn this off to keep everything on disk.",
                            )
                            .color(MUTED)
                            .size(11.0),
                        );
                        ui.add_space(6.0);

                        ui.label(
                            RichText::new("CHECK FOR CHANGES")
                                .size(10.5)
                                .color(MUTED)
                                .strong(),
                        );
                        ui.add(
                            egui::Slider::new(&mut draft.poll_interval_secs, 10..=600)
                                .suffix(" s")
                                .text("interval"),
                        );
                        ui.add_space(6.0);

                        ui.label(
                            RichText::new("FOLDERS TO SYNC")
                                .size(10.5)
                                .color(MUTED)
                                .strong(),
                        );
                        if folders.is_empty() {
                            ui.label(
                                RichText::new(
                                    "The folder list appears once the first sync has listed \
                                     your drive.",
                                )
                                .color(MUTED)
                                .size(11.0),
                            );
                        } else {
                            // An empty sync_folders means "everything", so the
                            // "All folders" tick and the list are two views of
                            // the same value rather than two settings.
                            let mut all = draft.sync_folders.is_empty();
                            if ui.checkbox(&mut all, "All folders").changed() {
                                draft.sync_folders = if all {
                                    Vec::new()
                                } else {
                                    // Start from everything ticked, so nothing
                                    // vanishes until something is deselected.
                                    folders.clone()
                                };
                            }
                            if !draft.sync_folders.is_empty() {
                                ui.indent("folder_list", |ui| {
                                    for folder in folders.iter() {
                                        let mut on = draft.sync_folders.contains(folder);
                                        if ui.checkbox(&mut on, folder).changed() {
                                            if on {
                                                draft.sync_folders.push(folder.clone());
                                            } else {
                                                draft.sync_folders.retain(|f| f != folder);
                                            }
                                            // Deselecting the last one would
                                            // mean "everything" — keep it on.
                                            if draft.sync_folders.is_empty() {
                                                draft.sync_folders.push(folder.clone());
                                            }
                                        }
                                    }
                                });
                                ui.label(
                                    RichText::new(
                                        "Folders you turn off are removed from this computer. \
                                         They stay on OneDrive, and come back if you turn them \
                                         on again.",
                                    )
                                    .color(WARN)
                                    .size(11.0),
                                );
                            }
                        }
                        ui.add_space(6.0);

                        ui.label(
                            RichText::new("KEEP AT MOST")
                                .size(10.5)
                                .color(MUTED)
                                .strong(),
                        );
                        ui.add(
                            egui::Slider::new(&mut draft.max_cache_size_gb, 0.0..=200.0)
                                .suffix(" GB")
                                .text("on disk"),
                        );
                        ui.label(
                            RichText::new(
                                "Files you open are kept for next time. Past this size the ones \
                                 you have not touched in a while are removed again — never \
                                 pinned files, and never anything still waiting to upload. \
                                 0 means no limit.",
                            )
                            .color(MUTED)
                            .size(11.0),
                        );
                        ui.add_space(6.0);

                        ui.label(
                            RichText::new("SIGN-IN METHOD")
                                .size(10.5)
                                .color(MUTED)
                                .strong(),
                        );
                        for (id, label) in AUTH_METHODS {
                            ui.radio_value(&mut draft.auth_method, id.to_string(), label);
                        }
                        ui.label(
                            RichText::new(
                                "Choose Browser sign-in if your organisation blocks the device \
                                 code flow (error AADSTS53003).",
                            )
                            .color(MUTED)
                            .size(11.0),
                        );
                        ui.add_space(6.0);

                        ui.label(RichText::new("EXCLUDE").size(10.5).color(MUTED).strong());
                        ui.add(
                            egui::TextEdit::multiline(&mut draft.excluded_patterns)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.label(
                            RichText::new(
                                "One glob pattern per line. These files are never synced.",
                            )
                            .color(MUTED)
                            .size(11.0),
                        );
                        ui.add_space(6.0);

                        if ui.link("Open config.toml in a text editor").clicked() {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(settings::config_path())
                                .spawn();
                        }

                        if let Some(result) = status {
                            ui.add_space(6.0);
                            match result {
                                Ok(msg) => {
                                    ui.label(RichText::new(msg.as_str()).color(GOOD).size(12.0))
                                }
                                Err(msg) => {
                                    ui.label(RichText::new(msg.as_str()).color(BAD).size(12.0))
                                }
                            };
                        }
                    });

                egui::TopBottomPanel::bottom("settings_actions")
                    .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(0.0, 8.0)))
                    .show_inside(ui, |ui| {
                        ui.separator();
                        let changed = draft != original;
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(changed, egui::Button::new("Save & restart"))
                                .clicked()
                            {
                                save = true;
                            }
                            if ui
                                .add_enabled(changed, egui::Button::new("Discard"))
                                .clicked()
                            {
                                *draft = original.clone();
                                *status = None;
                            }
                            if !changed {
                                ui.label(RichText::new("No changes").color(MUTED).size(11.5));
                            }
                        });
                    });
            });

            if save {
                // Saving alone changes nothing: the daemon reads the config
                // once at startup, so it has to be restarted to pick it up.
                *status = Some(match settings::save(draft) {
                    Ok(()) if settings::restart_daemon() => {
                        *original = draft.clone();
                        Ok("Saved — OneDrive restarted with the new settings.".into())
                    }
                    Ok(()) => {
                        *original = draft.clone();
                        Err(
                            "Saved, but OneDrive could not be restarted. Restart it yourself \
                             for the changes to take effect."
                                .into(),
                        )
                    }
                    Err(e) => Err(format!("Could not save: {e}")),
                });
            }
            if close {
                self.view = View::Status;
            }
            return;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ── Header ────────────────────────────────────────────────
            let (color, text) = self.headline();
            let mut sign_in_clicked = false;
            let mut settings_clicked = false;
            let mut toggle_pin: Option<(String, bool)> = None;
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

            // ── Conflicts ─────────────────────────────────────────────
            // Shown above recent activity because it is the one thing here
            // that needs a decision from the user rather than just informing
            // them. Both versions exist on disk; nothing is lost either way.
            if !s.conflicts.is_empty() {
                ui.label(
                    RichText::new("CHANGED IN BOTH PLACES")
                        .size(10.5)
                        .color(BAD)
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "These were edited here and on OneDrive. Your version was kept \
                         alongside the one from OneDrive — open them and keep whichever \
                         you want.",
                    )
                    .color(MUTED)
                    .size(11.0),
                );
                for (path, kept) in s.conflicts.clone().iter().take(5) {
                    let name = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&name).strong().size(12.5).color(BAD));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Show").clicked() {
                                // Select the file in the file manager rather
                                // than opening it — the point is to compare.
                                let target = std::path::Path::new(path)
                                    .parent()
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or_default();
                                let _ = std::process::Command::new("xdg-open").arg(target).spawn();
                            }
                            if !kept.is_empty() && ui.small_button("Your copy").clicked() {
                                let _ = std::process::Command::new("xdg-open").arg(kept).spawn();
                            }
                        });
                    });
                }
                if s.conflicts.len() > 5 {
                    ui.label(
                        RichText::new(format!("… and {} more", s.conflicts.len() - 5))
                            .color(MUTED)
                            .size(11.0),
                    );
                }
                ui.separator();
            }

            // ── On this device ────────────────────────────────────────
            // Files On-Demand is about controlling what is on disk, and until
            // now the only ways to do that were Dolphin's context menu and
            // odctl. Both live outside the app that owns the setting.
            let mut free_space_clicked = false;
            let mut unpin: Option<String> = None;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("ON THIS DEVICE")
                        .size(10.5)
                        .color(MUTED)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            s.cache_usage > 0,
                            egui::Button::new("Free up space").small(),
                        )
                        .on_hover_text(
                            "Removes downloaded copies that are safe to remove. Pinned files \
                             and anything still waiting to upload are kept.",
                        )
                        .clicked()
                    {
                        free_space_clicked = true;
                    }
                    ui.label(
                        RichText::new(human_bytes(s.cache_usage))
                            .color(MUTED)
                            .size(11.5),
                    );
                });
            });

            if let Some((freed, at)) = self.freed_notice {
                if at.elapsed() < Duration::from_secs(8) {
                    let text = if freed > 0 {
                        format!("Freed {}", human_bytes(freed))
                    } else {
                        "Nothing could be freed — everything cached is pinned, waiting to \
                         upload, or in use."
                            .to_string()
                    };
                    let colour = if freed > 0 { GOOD } else { MUTED };
                    ui.label(RichText::new(text).color(colour).size(11.0));
                }
            }

            if s.pinned.is_empty() {
                ui.label(
                    RichText::new(
                        "Nothing is pinned. Right-click a file in your file manager and choose \
                         \"Always keep on this device\" to keep it available offline.",
                    )
                    .color(MUTED)
                    .size(11.0),
                );
            } else {
                for (path, size) in s.pinned.clone().iter().take(4) {
                    let name = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("📌").size(11.0));
                        ui.label(RichText::new(&name).size(12.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("Unpin")
                                .on_hover_text("Stop keeping this on the device")
                                .clicked()
                            {
                                unpin = Some(path.clone());
                            }
                            ui.label(RichText::new(human_bytes(*size)).color(MUTED).size(11.0));
                        });
                    });
                }
                if s.pinned.len() > 4 {
                    ui.label(
                        RichText::new(format!("… and {} more pinned", s.pinned.len() - 4))
                            .color(MUTED)
                            .size(11.0),
                    );
                }
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
                for (full_path, name, parent, state, ts) in s.recent.clone() {
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
                            // Acting on a file you just saw change is the most
                            // likely reason to open this window at all.
                            let is_pinned = state == "Pinned";
                            let label = if is_pinned { "Unpin" } else { "Pin" };
                            if ui
                                .small_button(label)
                                .on_hover_text(if is_pinned {
                                    "Stop keeping this on the device"
                                } else {
                                    "Always keep this on the device"
                                })
                                .clicked()
                            {
                                toggle_pin = Some((full_path.clone(), !is_pinned));
                            }
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
                            settings_clicked = true;
                        }
                    });
                });

            if free_space_clicked {
                self.freed_notice = Some((self.client.free_up_space(), Instant::now()));
                self.snap = self.client.fetch();
            }
            if let Some(path) = unpin {
                self.client.set_pinned(&path, false);
                self.snap = self.client.fetch();
            }
            if let Some((path, pin)) = toggle_pin {
                self.client.set_pinned(&path, pin);
                self.snap = self.client.fetch();
            }
            if settings_clicked {
                self.open_settings();
            }
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
