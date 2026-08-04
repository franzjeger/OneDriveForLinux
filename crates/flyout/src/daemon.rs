//! Blocking D-Bus client for the daemon, polled from the UI thread on a
//! short interval. All calls degrade gracefully when the daemon is away.

use zbus::blocking::Connection;
use zbus::proxy;

#[proxy(
    interface = "com.onedrive.linux1",
    default_service = "com.onedrive.linux1",
    default_path = "/com/onedrive/linux1"
)]
pub trait OneDriveControl {
    fn get_state_counts(&self) -> zbus::Result<Vec<(String, u32)>>;
    fn is_paused(&self) -> zbus::Result<bool>;
    fn get_quota(&self) -> zbus::Result<(u64, u64)>;
    fn get_recent(&self) -> zbus::Result<Vec<(String, String, i64)>>;
    fn pause(&self) -> zbus::Result<()>;
    fn resume(&self) -> zbus::Result<()>;
    fn needs_auth(&self) -> zbus::Result<bool>;
    fn get_progress(&self) -> zbus::Result<String>;
    fn pending_uploads(&self) -> zbus::Result<u32>;
    fn top_level_folders(&self) -> zbus::Result<Vec<String>>;
    fn get_conflicts(&self) -> zbus::Result<Vec<(String, String)>>;
    fn start_auth(&self) -> zbus::Result<(String, String, String)>;
}

/// One UI-ready snapshot of everything the window shows.
#[derive(Default, Clone)]
pub struct Snapshot {
    pub reachable: bool,
    pub paused: bool,
    pub needs_auth: bool,
    pub total_items: usize,
    pub syncing: usize,
    pub errors: usize,
    pub quota_used: u64,
    pub quota_total: u64,
    /// Live progress line from the engine, empty when no pass is running.
    pub progress: String,
    /// Edits queued for upload that have not reached OneDrive yet.
    pub pending_uploads: u32,
    /// Files changed in both places: (path in the mount, preserved local copy).
    pub conflicts: Vec<(String, String)>,
    /// (file name, parent dir, state, unix seconds)
    pub recent: Vec<(String, String, String, i64)>,
}

/// systemd user unit installed by install.sh.
const SERVICE: &str = "onedrive-linux.service";

pub struct DaemonClient {
    proxy: Option<OneDriveControlProxyBlocking<'static>>,
}

impl DaemonClient {
    pub fn connect() -> Self {
        let proxy = Connection::session()
            .ok()
            .and_then(|conn| OneDriveControlProxyBlocking::new(&conn).ok());
        Self { proxy }
    }

    /// Ask systemd to start the background service.
    ///
    /// Launching the app from the desktop menu must not require the user to
    /// have opened a terminal first, so the window brings its own daemon up.
    /// The unit is `Type=simple`, so this returns as soon as the process is
    /// forked; the D-Bus name lands a moment later and the UI's normal refresh
    /// picks it up.
    pub fn start_daemon(&self) -> bool {
        std::process::Command::new("systemctl")
            .args(["--user", "start", SERVICE])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Whether the daemon is currently answering on D-Bus.
    pub fn reachable(&self) -> bool {
        self.proxy
            .as_ref()
            .is_some_and(|proxy| proxy.is_paused().is_ok())
    }

    pub fn fetch(&self) -> Snapshot {
        let Some(proxy) = &self.proxy else {
            return Snapshot::default();
        };
        // Counts, not the item table: this runs every couple of seconds, and a
        // large drive has hundreds of thousands of items.
        let Ok(counts) = proxy.get_state_counts() else {
            return Snapshot::default();
        };

        let mut snap = Snapshot {
            reachable: true,
            paused: proxy.is_paused().unwrap_or(false),
            needs_auth: proxy.needs_auth().unwrap_or(false),
            progress: proxy.get_progress().unwrap_or_default(),
            pending_uploads: proxy.pending_uploads().unwrap_or(0),
            ..Default::default()
        };
        for (state, count) in &counts {
            let count = *count as usize;
            snap.total_items += count;
            // Keyed on the stored state name, which is stable, rather than on
            // the human-readable text, which is not.
            match state.as_str() {
                "syncing" => snap.syncing += count,
                "error" | "conflict" => snap.errors += count,
                _ => {}
            }
        }
        // Only fetched when something is actually conflicted, so the common
        // case costs nothing.
        if snap.errors > 0 {
            snap.conflicts = proxy.get_conflicts().unwrap_or_default();
        }
        if let Ok((used, total)) = proxy.get_quota() {
            snap.quota_used = used;
            snap.quota_total = total;
        }
        if let Ok(recent) = proxy.get_recent() {
            snap.recent = recent
                .into_iter()
                .map(|(path, state, ts)| {
                    let p = std::path::Path::new(&path);
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    let parent = p
                        .parent()
                        .and_then(|d| d.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    (name, parent, state, ts)
                })
                .collect();
        }
        snap
    }

    /// Folder names at the top of the drive, for the settings view. Empty when
    /// the daemon is unreachable or the first sync has not listed them yet.
    pub fn top_level_folders(&self) -> Vec<String> {
        self.proxy
            .as_ref()
            .and_then(|p| p.top_level_folders().ok())
            .unwrap_or_default()
    }

    /// Kick off the device-code flow. Returns (message, user code, url).
    pub fn start_auth(&self) -> Option<(String, String, String)> {
        self.proxy.as_ref().and_then(|p| p.start_auth().ok())
    }

    pub fn set_paused(&self, pause: bool) {
        if let Some(proxy) = &self.proxy {
            let _ = if pause { proxy.pause() } else { proxy.resume() };
        }
    }
}

/// "just now", "12:40", "yesterday" — the shortest honest label.
pub fn relative_time(ts: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let age = now - ts;
    if age < 60 {
        "just now".into()
    } else if age < 24 * 3600 {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default()
    } else {
        format!("{}d ago", age / (24 * 3600))
    }
}

pub fn human_bytes(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = b as f64;
    if b >= 1000.0 * GB {
        format!("{:.1} TB", b / (1024.0 * GB))
    } else if b >= GB {
        format!("{:.0} GB", b / GB)
    } else {
        format!("{:.0} MB", b / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(500 * 1024 * 1024), "500 MB");
        assert_eq!(human_bytes(340 * 1024 * 1024 * 1024), "340 GB");
        assert_eq!(human_bytes(1024_u64.pow(4)), "1.0 TB");
    }

    #[test]
    fn relative_time_recent_is_just_now() {
        assert_eq!(relative_time(chrono::Utc::now().timestamp()), "just now");
    }
}
