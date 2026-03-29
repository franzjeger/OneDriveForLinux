use ksni::{menu::*, Tray};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Arc,
};
use sync_engine::{SyncEvent, SyncState};
use tracing::info;
use tokio::sync::broadcast;

const MAX_RECENT: usize = 5;

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
}

impl TrayStatus {
    fn icon_name(&self) -> &str {
        match self {
            TrayStatus::Idle => ICON_IDLE,
            TrayStatus::Syncing => ICON_SYNCING,
            TrayStatus::Error(_) => ICON_ERROR,
            TrayStatus::Paused => ICON_PAUSED,
        }
    }

    fn tooltip(&self) -> String {
        match self {
            TrayStatus::Idle => "OneDrive — Up to date".into(),
            TrayStatus::Syncing => "OneDrive — Syncing…".into(),
            TrayStatus::Error(e) => format!("OneDrive — Error: {e}"),
            TrayStatus::Paused => "OneDrive — Paused".into(),
        }
    }

    fn is_paused(&self) -> bool {
        matches!(self, TrayStatus::Paused)
    }
}

/// Shared tray state behind a Mutex so ksni callbacks can access it.
struct TrayState {
    status: TrayStatus,
    recent: VecDeque<(PathBuf, SyncState)>,
    sync_dir: PathBuf,
    config_path: PathBuf,
}

impl TrayState {
    fn new(sync_dir: PathBuf, config_path: PathBuf) -> Self {
        Self {
            status: TrayStatus::Idle,
            recent: VecDeque::with_capacity(MAX_RECENT),
            sync_dir,
            config_path,
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
    fn id(&self) -> String {
        "onedrive-linux".into()
    }

    fn title(&self) -> String {
        "OneDrive".into()
    }

    fn icon_name(&self) -> String {
        self.state.lock().status.icon_name().to_string()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let st = self.state.lock();
        ksni::ToolTip {
            icon_name: st.status.icon_name().to_string(),
            icon_pixmap: vec![],
            title: "OneDrive for Linux".into(),
            description: st.status.tooltip(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let st = self.state.lock();
        let sync_dir = st.sync_dir.clone();
        let config_path = st.config_path.clone();
        let is_paused = st.status.is_paused();
        let recent: Vec<_> = st.recent.iter().cloned().collect();
        drop(st); // release lock before building closures

        let mut items: Vec<MenuItem<Self>> = Vec::new();

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

        items.push(MenuItem::Separator);

        // Pause / Resume — read state fresh inside the closure
        let toggle_label = if is_paused { "Resume Sync" } else { "Pause Sync" };
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
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
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
                label: "Settings (Edit Config)".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(move |_| {
                    let _ = std::process::Command::new("xdg-open")
                        .arg(config_path.as_os_str())
                        .spawn();
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
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
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

    // Tokio task: receive sync events and update tray state
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let mut st = state.lock();
                    match event {
                        SyncEvent::SyncStarted => {
                            st.status = TrayStatus::Syncing;
                        }
                        SyncEvent::SyncCompleted => {
                            if !matches!(st.status, TrayStatus::Paused) {
                                st.status = TrayStatus::Idle;
                            }
                        }
                        SyncEvent::ItemStateChanged { path, state: sync_state } => {
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
                            st.status =
                                TrayStatus::Error("Authentication required".into());
                        }
                    }
                    drop(st);
                    handle.update(|_| {});
                }
                Err(_) => break, // channel closed / lagged
            }
        }
        info!("Tray event loop exiting");
    });

    Ok(())
}
