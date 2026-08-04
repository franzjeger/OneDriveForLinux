use graph_client::GraphClient;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use sync_engine::SyncEngine;
use tracing::info;
use zbus::{fdo::RequestNameFlags, interface, Connection};

/// Rolling buffer of recent item activity: (path, state, unix timestamp).
pub type RecentBuffer = Arc<Mutex<VecDeque<(String, String, i64)>>>;

/// Latest human-readable progress line from the engine, empty when idle.
pub type ProgressText = Arc<Mutex<String>>;

pub struct OneDriveInterface {
    pub engine: Arc<SyncEngine>,
    pub recent: RecentBuffer,
    pub needs_auth: Arc<std::sync::atomic::AtomicBool>,
    pub progress: ProgressText,
    pub quota: crate::quota::QuotaCache,
}

#[interface(name = "com.onedrive.linux1")]
impl OneDriveInterface {
    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.engine.pause().await;
        info!("D-Bus: Pause");
        Ok(())
    }

    async fn resume(&self) -> zbus::fdo::Result<()> {
        self.engine.resume().await;
        info!("D-Bus: Resume");
        Ok(())
    }

    async fn get_status(&self) -> zbus::fdo::Result<Vec<(String, String)>> {
        let status = self.engine.get_status().await;
        Ok(status
            .into_iter()
            .map(|(path, state)| (path.to_string_lossy().to_string(), state.to_string()))
            .collect())
    }

    /// Item counts keyed by the stored state name (`synced`, `cloud_only`,
    /// `syncing`, `error`, …). Status displays should use this rather than
    /// GetStatus: a large drive has hundreds of thousands of items, and
    /// shipping them all over the bus every couple of seconds to compute three
    /// numbers is not something the bus, or the database, should be asked to do.
    async fn get_state_counts(&self) -> zbus::fdo::Result<Vec<(String, u32)>> {
        Ok(self
            .engine
            .get_state_counts()
            .await
            .into_iter()
            .map(|(state, count)| (state, count.min(u32::MAX as u64) as u32))
            .collect())
    }

    /// Up to `limit` items that are syncing, failed, or in conflict — the ones
    /// worth naming in a status display.
    async fn get_attention_items(&self, limit: u32) -> zbus::fdo::Result<Vec<(String, String)>> {
        Ok(self
            .engine
            .get_attention_items(limit as usize)
            .await
            .into_iter()
            .map(|(path, state)| (path.to_string_lossy().to_string(), state.to_string()))
            .collect())
    }

    async fn force_sync(&self, path: String) -> zbus::fdo::Result<()> {
        info!("D-Bus: ForceSync {path}");
        let engine = Arc::clone(&self.engine);
        let p = std::path::PathBuf::from(path);
        tokio::spawn(async move {
            if let Err(e) = engine.upload_item(&p).await {
                tracing::error!("ForceSync error: {e}");
            }
        });
        Ok(())
    }

    async fn is_paused(&self) -> zbus::fdo::Result<bool> {
        Ok(self.engine.is_paused().await)
    }

    /// Current progress line ("Fetching file list… 1200 items"), or an empty
    /// string when no pass is running. Lets the flyout say what is happening
    /// during a long first sync instead of looking stalled.
    async fn get_progress(&self) -> zbus::fdo::Result<String> {
        Ok(self.progress.lock().clone())
    }

    /// How many uploads are queued for retry. Non-zero means edits exist that
    /// have not reached OneDrive yet.
    async fn pending_uploads(&self) -> zbus::fdo::Result<u32> {
        Ok(self.engine.pending_uploads().await as u32)
    }

    /// True when the daemon is waiting for the user to re-authenticate.
    async fn needs_auth(&self) -> zbus::fdo::Result<bool> {
        Ok(self.needs_auth.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Storage quota as (used, total) bytes. (0, 0) when unknown.
    /// Used and total bytes on the drive, served from a short-lived cache.
    /// Status displays poll this every couple of seconds; asking Graph each
    /// time would be thousands of requests an hour against a throttled API for
    /// a number that barely moves.
    async fn get_quota(&self) -> zbus::fdo::Result<(u64, u64)> {
        Ok(self.quota.get().await)
    }

    /// Most recent item activity, newest first: (path, state, unix seconds).
    async fn get_recent(&self) -> zbus::fdo::Result<Vec<(String, String, i64)>> {
        Ok(self.recent.lock().iter().rev().cloned().collect())
    }

    /// Mark a file or folder as always-on-device and download it immediately.
    async fn pin_item(&self, path: String) -> zbus::fdo::Result<()> {
        info!("D-Bus: PinItem {path}");
        let engine = Arc::clone(&self.engine);
        let p = std::path::PathBuf::from(path);
        tokio::spawn(async move {
            if let Err(e) = engine.pin_item(&p).await {
                tracing::error!("PinItem error: {e}");
            }
        });
        Ok(())
    }

    /// Start the OAuth2 device code re-authentication flow.
    /// Returns (message, user_code, verification_uri) to display to the user.
    /// The daemon polls for the token in background and auto-resumes sync when done.
    async fn start_auth(&self) -> zbus::fdo::Result<(String, String, String)> {
        info!("D-Bus: StartAuth");
        Arc::clone(&self.engine)
            .start_reauthenticate()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Remove pin from a file or folder, free cache space, convert to cloud-only.
    async fn unpin_item(&self, path: String) -> zbus::fdo::Result<()> {
        info!("D-Bus: UnpinItem {path}");
        let engine = Arc::clone(&self.engine);
        let p = std::path::PathBuf::from(path);
        tokio::spawn(async move {
            if let Err(e) = engine.unpin_item(&p).await {
                tracing::error!("UnpinItem error: {e}");
            }
        });
        Ok(())
    }
}

pub async fn export_dbus(
    engine: Arc<SyncEngine>,
    graph: Arc<GraphClient>,
    recent: RecentBuffer,
    needs_auth: Arc<std::sync::atomic::AtomicBool>,
    progress: ProgressText,
) -> anyhow::Result<Connection> {
    // The interface reaches Graph only through this cache.
    let quota = crate::quota::QuotaCache::new(graph);
    let conn = Connection::session().await?;
    conn.object_server()
        .at(
            "/com/onedrive/linux1",
            OneDriveInterface {
                engine,
                recent,
                needs_auth,
                progress,
                quota,
            },
        )
        .await?;

    // Request the well-known name.
    // - AllowReplacement: a future daemon instance can take over from us.
    // - ReplaceExisting:  forcibly replace any current holder that has set
    //   AllowReplacement (including zombie instances of ourselves).
    let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::ReplaceExisting;
    let reply = conn
        .request_name_with_flags("com.onedrive.linux1", flags)
        .await?;

    use zbus::fdo::RequestNameReply;
    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
            info!("D-Bus interface exported at com.onedrive.linux1");
        }
        RequestNameReply::InQueue => {
            // Previous holder did NOT set AllowReplacement — wait for it to exit.
            info!("D-Bus name queued — will become active when previous holder exits");
        }
        RequestNameReply::Exists => {
            anyhow::bail!("D-Bus name com.onedrive.linux1 already taken and not replaceable");
        }
    }

    Ok(conn)
}
