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
    fn get_status(&self) -> zbus::Result<Vec<(String, String)>>;
    fn is_paused(&self) -> zbus::Result<bool>;
    fn get_quota(&self) -> zbus::Result<(u64, u64)>;
    fn get_recent(&self) -> zbus::Result<Vec<(String, String, i64)>>;
    fn pause(&self) -> zbus::Result<()>;
    fn resume(&self) -> zbus::Result<()>;
}

/// One UI-ready snapshot of everything the window shows.
#[derive(Default, Clone)]
pub struct Snapshot {
    pub reachable: bool,
    pub paused: bool,
    pub total_items: usize,
    pub syncing: usize,
    pub errors: usize,
    pub quota_used: u64,
    pub quota_total: u64,
    /// (file name, parent dir, state, unix seconds)
    pub recent: Vec<(String, String, String, i64)>,
}

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

    pub fn fetch(&self) -> Snapshot {
        let Some(proxy) = &self.proxy else {
            return Snapshot::default();
        };
        let Ok(items) = proxy.get_status() else {
            return Snapshot::default();
        };

        let mut snap = Snapshot {
            reachable: true,
            paused: proxy.is_paused().unwrap_or(false),
            total_items: items.len(),
            ..Default::default()
        };
        for (_, state) in &items {
            match state.as_str() {
                "Syncing" => snap.syncing += 1,
                s if s.starts_with("Error") => snap.errors += 1,
                "Conflict" => snap.errors += 1,
                _ => {}
            }
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
