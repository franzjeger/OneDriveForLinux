mod icons;

use icons::IconState;
use ksni::{menu::*, Tray};
use parking_lot::Mutex;
use std::{collections::VecDeque, path::PathBuf, sync::Arc};
use sync_engine::{SyncEvent, SyncState};
use tokio::sync::broadcast;
use tracing::info;

const MAX_RECENT: usize = 5;

/// Longest error text shown in the tray menu/tooltip before truncation.
const MAX_ERROR_CHARS: usize = 90;

const ICON_IDLE: &str = "folder-cloud";
const ICON_SYNCING: &str = "emblem-synchronizing";
const ICON_ERROR: &str = "dialog-error";
const ICON_PAUSED: &str = "media-playback-pause";

#[derive(Debug, Clone, PartialEq)]
enum TrayStatus {
    Idle,
    Syncing,
    Error(String),
    Paused,
    AuthRequired,
}

impl TrayStatus {
    fn icon_state(&self) -> IconState {
        match self {
            TrayStatus::Idle => IconState::Ok,
            TrayStatus::Syncing => IconState::Syncing,
            TrayStatus::Error(_) => IconState::Error,
            TrayStatus::Paused => IconState::Paused,
            TrayStatus::AuthRequired => IconState::AuthRequired,
        }
    }

    fn icon_name(&self) -> &str {
        match self {
            TrayStatus::Idle => ICON_IDLE,
            TrayStatus::Syncing => ICON_SYNCING,
            TrayStatus::Error(_) => ICON_ERROR,
            TrayStatus::Paused => ICON_PAUSED,
            TrayStatus::AuthRequired => ICON_ERROR,
        }
    }

    fn tooltip(&self) -> String {
        match self {
            TrayStatus::Idle => "OneDrive — Up to date".into(),
            TrayStatus::Syncing => "OneDrive — Syncing…".into(),
            TrayStatus::Error(e) => format!("OneDrive — Error: {}", short_error(e)),
            TrayStatus::Paused => "OneDrive — Paused".into(),
            TrayStatus::AuthRequired => "OneDrive — Sign in required".into(),
        }
    }

    fn is_paused(&self) -> bool {
        matches!(self, TrayStatus::Paused)
    }
}

/// Graph errors carry the full JSON body for the log. Pasting that into a menu
/// item makes the tray unreadable, so keep the first sentence and cap it.
fn short_error(e: &str) -> String {
    let first_line = e.lines().next().unwrap_or(e).trim();
    // Cut at the JSON body if there is one — the prose before it is the useful part.
    let prose = first_line
        .split_once(": {")
        .map(|(before, _)| before)
        .unwrap_or(first_line);
    let mut out: String = prose.chars().take(MAX_ERROR_CHARS).collect();
    if prose.chars().count() > MAX_ERROR_CHARS {
        out.push('…');
    }
    out
}

/// Shared tray state behind a Mutex so ksni callbacks can access it.
struct TrayState {
    status: TrayStatus,
    /// Latest progress line from the engine ("Fetching file list… 1200 items").
    /// Replaces the generic "Syncing…" text so a slow pass doesn't look hung.
    detail: Option<String>,
    recent: VecDeque<(PathBuf, SyncState)>,
    sync_dir: PathBuf,
    config_path: PathBuf,
}

impl TrayState {
    fn new(sync_dir: PathBuf, config_path: PathBuf) -> Self {
        Self {
            status: TrayStatus::Idle,
            detail: None,
            recent: VecDeque::with_capacity(MAX_RECENT),
            sync_dir,
            config_path,
        }
    }

    /// Status text for the menu header and tooltip, preferring live progress.
    fn status_line(&self) -> String {
        match (&self.status, &self.detail) {
            (TrayStatus::Syncing, Some(detail)) => format!("OneDrive — {detail}"),
            (status, _) => status.tooltip(),
        }
    }

    fn push_recent(&mut self, path: PathBuf, state: SyncState) {
        self.recent.retain(|(p, _)| p != &path);
        if self.recent.len() >= MAX_RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((path, state));
    }
}

pub struct OneDriveTray {
    state: Arc<Mutex<TrayState>>,
}

impl OneDriveTray {
    pub fn new(sync_dir: PathBuf, config_path: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(TrayState::new(sync_dir, config_path))),
        }
    }
}

impl Tray for OneDriveTray {
    fn activate(&mut self, _x: i32, _y: i32) {
        // Left click opens the status flyout window. --flyout asks it to
        // behave like a panel popup (dismiss when focus is lost) rather than
        // like the ordinary window the application menu launches.
        let _ = std::process::Command::new("onedrive-flyout")
            .arg("--flyout")
            .spawn();
    }

    fn id(&self) -> String {
        "onedrive-linux".into()
    }

    fn title(&self) -> String {
        "OneDrive".into()
    }

    fn icon_name(&self) -> String {
        // Themed fallback for hosts that ignore pixmaps.
        self.state.lock().status.icon_name().to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icons::render(self.state.lock().status.icon_state())
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let st = self.state.lock();
        ksni::ToolTip {
            icon_name: st.status.icon_name().to_string(),
            icon_pixmap: vec![],
            title: "OneDrive for Linux".into(),
            description: st.status_line(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let st = self.state.lock();
        let sync_dir = st.sync_dir.clone();
        let config_path = st.config_path.clone();
        let is_paused = st.status.is_paused();
        let auth_required = matches!(st.status, TrayStatus::AuthRequired);
        let recent: Vec<_> = st.recent.iter().cloned().collect();
        drop(st); // release lock before building closures

        let mut items: Vec<MenuItem<Self>> = Vec::new();

        // Status header — the first thing the menu says is the app's state.
        let status_line = {
            let st = self.state.lock();
            st.status_line()
        };
        items.push(
            StandardItem {
                label: status_line,
                enabled: false,
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);

        // Open folder
        items.push(
            StandardItem {
                label: "Open OneDrive Folder".into(),
                icon_name: "folder".into(),
                activate: Box::new(move |_this: &mut OneDriveTray| {
                    let _ = std::process::Command::new("xdg-open")
                        .arg(sync_dir.as_os_str())
                        .spawn();
                }),
                ..Default::default()
            }
            .into(),
        );

        // Sign in again — shown only when authentication has failed
        if auth_required {
            items.push(MenuItem::Separator);
            items.push(
                StandardItem {
                    label: "Sign in again…".into(),
                    icon_name: "dialog-password".into(),
                    activate: Box::new(|_| {
                        // The flyout renders the device code with copy/open buttons.
                        let _ = std::process::Command::new("onedrive-flyout")
                            .arg("--signin")
                            .spawn();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);

        // Pause / Resume — read state fresh inside the closure
        let toggle_label = if is_paused {
            "Resume Sync"
        } else {
            "Pause Sync"
        };
        let toggle_icon = if is_paused {
            "media-playback-start"
        } else {
            "media-playback-pause"
        };
        items.push(
            StandardItem {
                label: toggle_label.into(),
                icon_name: toggle_icon.into(),
                activate: Box::new(move |_this: &mut OneDriveTray| {
                    let cmd = if is_paused { "resume" } else { "pause" };
                    let _ = std::process::Command::new("odctl").arg(cmd).spawn();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Recent activity
        if recent.is_empty() {
            items.push(
                StandardItem {
                    label: "(No recent activity)".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            for (path, sync_state) in recent {
                let label = format!(
                    "{} — {sync_state}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                );
                items.push(
                    StandardItem {
                        label,
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        items.push(MenuItem::Separator);

        // Settings
        items.push(
            StandardItem {
                label: "Settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(move |_| {
                    // The flyout has a real settings view. Fall back to opening
                    // the raw file only if that binary is missing.
                    if std::process::Command::new("onedrive-flyout")
                        .arg("--settings")
                        .spawn()
                        .is_err()
                    {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(config_path.as_os_str())
                            .spawn();
                    }
                }),
                ..Default::default()
            }
            .into(),
        );

        // Sign out
        items.push(
            StandardItem {
                label: "Sign Out".into(),
                icon_name: "system-log-out".into(),
                activate: Box::new(|_| {
                    let _ = std::process::Command::new("odctl")
                        .args(["auth", "--signout"])
                        .spawn();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Quit
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| {
                    // Raise SIGTERM instead of exiting directly so the daemon's
                    // signal handler runs its graceful shutdown (FUSE unmount,
                    // PID-file cleanup). A bare exit() leaves a ghost mount.
                    unsafe {
                        libc::raise(libc::SIGTERM);
                    }
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Render every icon state to `dir` as PNGs (dev tool — see examples/render_icons).
pub fn render_icon_previews(dir: &std::path::Path) -> anyhow::Result<()> {
    use icons::IconState;
    for (state, name) in [
        (IconState::Ok, "up-to-date"),
        (IconState::Syncing, "syncing"),
        (IconState::Paused, "paused"),
        (IconState::AuthRequired, "sign-in-needed"),
        (IconState::Error, "error"),
    ] {
        for icon in icons::render(state) {
            let mut pixmap = tiny_skia::Pixmap::new(icon.width as u32, icon.height as u32)
                .ok_or_else(|| anyhow::anyhow!("pixmap alloc"))?;
            // Convert ARGB network order back to RGBA for PNG encoding.
            for (dst, src) in pixmap
                .data_mut()
                .chunks_exact_mut(4)
                .zip(icon.data.chunks_exact(4))
            {
                dst.copy_from_slice(&[src[1], src[2], src[3], src[0]]);
            }
            pixmap.save_png(dir.join(format!("{name}-{}.png", icon.width)))?;
        }
    }
    Ok(())
}

/// Spawn the tray in the background, listening to SyncEvents and updating the icon/menu.
pub fn spawn_tray(
    sync_dir: PathBuf,
    config_path: PathBuf,
    mut event_rx: broadcast::Receiver<SyncEvent>,
) -> anyhow::Result<()> {
    let tray = OneDriveTray::new(sync_dir, config_path);
    let state = Arc::clone(&tray.state);

    let service = ksni::TrayService::new(tray);
    let handle = service.handle();

    // Spawn ksni event loop on its own OS thread (it is !Send)
    std::thread::spawn(move || {
        service.spawn_without_dbus_name();
    });

    // A full first sync emits an event per item. Pushing each one over D-Bus
    // would swamp the panel, so events only mark the tray dirty and a ticker
    // repaints at a human-visible rate.
    let dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let dirty = Arc::clone(&dirty);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                ticker.tick().await;
                if dirty.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    handle.update(|_| {});
                }
            }
        });
    }

    // Tokio task: receive sync events and update tray state
    tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                // Lagged: the engine outran us during a burst. State is
                // rebuilt from later events, so keep going.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    info!("Tray lagged {n} sync events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let mut st = state.lock();
            match event {
                SyncEvent::SyncStarted => {
                    st.status = TrayStatus::Syncing;
                    st.detail = None;
                }
                SyncEvent::SyncProgress(msg) => {
                    // Progress implies a pass is running, even if SyncStarted
                    // was missed (e.g. the tray attached mid-pass).
                    if !matches!(st.status, TrayStatus::Paused) {
                        st.status = TrayStatus::Syncing;
                    }
                    st.detail = Some(msg);
                }
                SyncEvent::SyncCompleted => {
                    st.detail = None;
                    if !matches!(st.status, TrayStatus::Paused) {
                        st.status = TrayStatus::Idle;
                    }
                }
                SyncEvent::ItemStateChanged {
                    path,
                    state: sync_state,
                } => {
                    st.push_recent(path, sync_state);
                }
                SyncEvent::Paused => {
                    st.status = TrayStatus::Paused;
                }
                SyncEvent::Resumed => {
                    st.status = TrayStatus::Idle;
                }
                SyncEvent::Error(msg) => {
                    st.status = TrayStatus::Error(msg);
                }
                SyncEvent::AuthRequired => {
                    st.status = TrayStatus::AuthRequired;
                }
            }
            drop(st);
            dirty.store(true, std::sync::atomic::Ordering::Release);
        }
        info!("Tray event loop exiting");
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_error_strips_the_json_body() {
        let raw = "API error 410: Gone: {\"error\":{\"code\":\"resyncRequired\",\"message\":\"Resync required. Replace any local items with the server's version\"}}";
        let short = short_error(raw);
        assert!(!short.contains('{'), "JSON body leaked into {short:?}");
        assert!(short.starts_with("API error 410: Gone"));
    }

    #[test]
    fn short_error_caps_long_prose() {
        let short = short_error(&"x".repeat(500));
        assert!(short.chars().count() <= MAX_ERROR_CHARS + 1);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn progress_replaces_the_generic_syncing_line() {
        let mut st = TrayState::new("/sync".into(), "/cfg".into());
        st.status = TrayStatus::Syncing;
        assert_eq!(st.status_line(), "OneDrive — Syncing…");
        st.detail = Some("Fetching file list… 1200 items".into());
        assert_eq!(
            st.status_line(),
            "OneDrive — Fetching file list… 1200 items"
        );
    }

    #[test]
    fn progress_does_not_leak_into_other_states() {
        let mut st = TrayState::new("/sync".into(), "/cfg".into());
        st.detail = Some("Fetching file list… 1200 items".into());
        st.status = TrayStatus::Paused;
        assert_eq!(st.status_line(), "OneDrive — Paused");
    }
}
