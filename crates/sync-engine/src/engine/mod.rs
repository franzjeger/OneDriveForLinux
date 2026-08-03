//! Sync engine core: construction, lifecycle, and shared helpers.
//!
//! The engine's responsibilities are split across submodules:
//! - [`delta`]  — remote delta sync, download, reconciliation, cache cleanup
//! - [`local`]  — local filesystem watcher and uploads
//! - [`pin`]    — pin/unpin (always-on-device) handling
use crate::{
    config::Config,
    db::{Database, DbItem},
    state::{SyncEvent, SyncState},
    watcher::{EventDebouncer, LocalWatcher},
};
use chrono::Utc;
use dashmap::DashMap;
use graph_client::{AuthManager, DeltaResponse, DriveItem, GraphClient, GraphError};
use std::{
    path::Path,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::{broadcast, Mutex as TokioMutex, RwLock, Semaphore};
use tracing::{debug, error, info, warn};

mod delta;
mod local;
mod pin;
mod uploads;
pub use uploads::retry_delay;

pub struct SyncEngine {
    config: Arc<Config>,
    db: Arc<Database>,
    graph: Arc<GraphClient>,
    auth: Arc<AuthManager>,
    event_tx: broadcast::Sender<SyncEvent>,
    paused: Arc<RwLock<bool>>,
    /// Cache directory for on-demand file storage (outside the FUSE mountpoint).
    cache_dir: Option<std::path::PathBuf>,
    /// Per-item locks to prevent concurrent downloads/uploads of the same file.
    /// Key: OneDrive item ID. The lock is held for the duration of the operation.
    item_locks: Arc<DashMap<String, Arc<TokioMutex<()>>>>,
}

impl SyncEngine {
    pub fn new(
        config: Arc<Config>,
        db: Arc<Database>,
        graph: Arc<GraphClient>,
        auth: Arc<AuthManager>,
        cache_dir: Option<std::path::PathBuf>,
    ) -> (Self, broadcast::Receiver<SyncEvent>) {
        let (event_tx, event_rx) = broadcast::channel(256);
        let engine = SyncEngine {
            config,
            db,
            graph,
            auth,
            event_tx,
            paused: Arc::new(RwLock::new(false)),
            cache_dir,
            item_locks: Arc::new(DashMap::new()),
        };
        (engine, event_rx)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }

    pub async fn pause(&self) {
        *self.paused.write().await = true;
        if let Err(e) = self.event_tx.send(SyncEvent::Paused) {
            warn!("Failed to send Paused event: {e}");
        }
        info!("Sync paused");
    }

    pub async fn resume(&self) {
        *self.paused.write().await = false;
        if let Err(e) = self.event_tx.send(SyncEvent::Resumed) {
            warn!("Failed to send Resumed event: {e}");
        }
        info!("Sync resumed");
    }

    pub async fn is_paused(&self) -> bool {
        *self.paused.read().await
    }

    /// Spawn background tasks: remote delta poller + local filesystem watcher.
    pub async fn start(self: Arc<Self>) -> anyhow::Result<()> {
        let engine_remote = Arc::clone(&self);
        let engine_local = Arc::clone(&self);

        // Remote watcher task — with restart-on-panic, matching local watcher.
        tokio::spawn(async move {
            loop {
                let engine_clone = Arc::clone(&engine_remote);
                let result = tokio::task::spawn(async move {
                    engine_clone.remote_watcher_loop().await;
                })
                .await;
                match result {
                    Ok(()) => {
                        warn!("Remote watcher exited normally — restarting in 5s");
                    }
                    Err(e) => {
                        error!("Remote watcher panicked: {e} — restarting in 5s");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });

        // Local watcher task — only in non-on-demand mode.
        // When on_demand=true, sync_dir is a FUSE mountpoint; inotify events
        // from FUSE reads would cause us to re-upload every file we serve.
        if !engine_local.config.on_demand {
            tokio::spawn(async move {
                loop {
                    let engine_clone = Arc::clone(&engine_local);
                    let result =
                        tokio::task::spawn(async move { engine_clone.local_watcher_loop().await })
                            .await;
                    match result {
                        Ok(Ok(())) => {
                            warn!("Local watcher exited normally — restarting in 5s");
                        }
                        Ok(Err(e)) => {
                            error!("Local watcher error: {e} — restarting in 5s");
                        }
                        Err(e) => {
                            error!("Local watcher panicked: {e} — restarting in 5s");
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
        }

        // Upload retry queue — drains failed uploads regardless of mode, since
        // both the FUSE write path and the local watcher feed it.
        let engine_uploads = Arc::clone(&self);
        tokio::spawn(async move {
            engine_uploads.upload_retry_loop().await;
        });

        info!("SyncEngine started");
        Ok(())
    }

    // ── Remote polling loop ────────────────────────────────────────────────────

    async fn remote_watcher_loop(self: Arc<Self>) {
        // Only announce a connectivity change, not every poll while it holds.
        let mut offline = false;
        loop {
            if !self.is_paused().await {
                match self.run_delta_sync().await {
                    Ok(()) => {
                        if offline {
                            info!("Network is back — resuming normal polling");
                            offline = false;
                            let _ = self.event_tx.send(SyncEvent::BackOnline);
                        }
                    }
                    Err(e) if Self::is_offline_error(&e) => {
                        // Being offline is not a fault to report: there is
                        // nothing for the user to fix and it resolves itself.
                        if !offline {
                            warn!("Network unreachable — sync will resume automatically");
                            offline = true;
                            let _ = self.event_tx.send(SyncEvent::Offline);
                        } else {
                            debug!("Still offline: {e}");
                        }
                    }
                    Err(e) => {
                        error!("Delta sync error: {e}");
                        offline = false;
                        if self.is_auth_error(&e) {
                            error!("Authentication failed — pausing sync. Run `onedrive-linux auth` to re-authenticate.");
                            self.pause().await;
                            let _ = self.event_tx.send(SyncEvent::AuthRequired);
                        } else {
                            let _ = self.event_tx.send(SyncEvent::Error(e.to_string()));
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(
                self.config.delta_poll_interval_secs,
            ))
            .await;
        }
    }

    /// Returns true if the error is "the network is not reachable" rather than
    /// a real failure — a DNS failure, a refused connection, or a timeout.
    pub(crate) fn is_offline_error(e: &anyhow::Error) -> bool {
        let Some(GraphError::Http(req)) = e.downcast_ref::<GraphError>() else {
            return false;
        };
        req.is_connect() || req.is_timeout()
    }

    /// Returns true if the error is an authentication/authorization failure.
    fn is_auth_error(&self, e: &anyhow::Error) -> bool {
        if let Some(ge) = e.downcast_ref::<GraphError>() {
            return matches!(ge, GraphError::Auth(_) | GraphError::TokenRefresh(_));
        }
        false
    }

    /// Trigger re-authentication via device code flow.
    /// Returns (message, user_code, verification_uri) for display to the user.
    /// Polls for the token in the background and auto-resumes sync when done.
    pub async fn start_reauthenticate(self: Arc<Self>) -> anyhow::Result<(String, String, String)> {
        // Browser sign-in has no user code — the caller shows a "finish in
        // your browser" message instead of a code card.
        if self.config.auth_preference() == Some(true) {
            let auth = Arc::clone(&self.auth);
            let engine = Arc::clone(&self);
            tokio::spawn(async move {
                match auth.authenticate_browser().await {
                    Ok(()) => {
                        info!("Browser re-authentication complete — resuming sync");
                        engine.resume().await;
                    }
                    Err(e) => {
                        error!("Browser re-authentication failed: {e}");
                        let _ = engine
                            .event_tx
                            .send(SyncEvent::Error(format!("Re-auth failed: {e}")));
                    }
                }
            });
            return Ok((
                "Finish signing in in your browser — sync resumes automatically.".to_string(),
                String::new(),
                String::new(),
            ));
        }

        let dc = self.auth.start_device_code_flow().await?;
        let info = (
            dc.message.clone(),
            dc.user_code.clone(),
            dc.verification_uri.clone(),
        );

        let auth = Arc::clone(&self.auth);
        let engine = Arc::clone(&self);
        tokio::spawn(async move {
            match auth.complete_device_auth(dc).await {
                Ok(()) => {
                    info!("Re-authentication complete — resuming sync");
                    engine.resume().await;
                }
                Err(e) => {
                    error!("Re-authentication failed: {e}");
                    let _ = engine
                        .event_tx
                        .send(SyncEvent::Error(format!("Re-auth failed: {e}")));
                }
            }
        });

        Ok(info)
    }
}

impl SyncEngine {
    pub async fn get_status(&self) -> Vec<(PathBuf, SyncState)> {
        self.db
            .all_items()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|i| (i.local_path, i.sync_state))
            .collect()
    }

    /// Minimum free disk space (100 MB) required before starting a download.
    const MIN_FREE_BYTES: u64 = 100 * 1024 * 1024;

    /// Check that the filesystem containing `path` has enough free space.
    /// Returns an error if free space is below MIN_FREE_BYTES.
    fn check_disk_space(path: &Path) -> anyhow::Result<()> {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(Path::new("/"))
        };
        let c_path = std::ffi::CString::new(dir.to_string_lossy().as_bytes())
            .unwrap_or_else(|_| std::ffi::CString::new("/").unwrap());
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                // POSIX: capacity math uses the fragment size f_frsize;
                // f_bsize is only the preferred I/O size.
                let frsize = if stat.f_frsize > 0 {
                    stat.f_frsize
                } else {
                    stat.f_bsize
                };
                let free = stat.f_bavail * frsize;
                if free < Self::MIN_FREE_BYTES {
                    let free_mb = free / (1024 * 1024);
                    anyhow::bail!(
                        "Low disk space: {free_mb} MB free on {dir:?} (need at least {} MB)",
                        Self::MIN_FREE_BYTES / (1024 * 1024)
                    );
                }
            }
        }
        Ok(())
    }

    /// Get or create a per-item lock. Prevents concurrent downloads/uploads
    /// of the same file, eliminating TOCTOU races on cache files.
    fn item_lock(&self, item_id: &str) -> Arc<TokioMutex<()>> {
        self.item_locks
            .entry(item_id.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    // ── Helpers ────────────────────────────────────────────────────────────────

    fn item_local_path(&self, item: &DriveItem) -> anyhow::Result<PathBuf> {
        // Try to build path from parent reference
        if let Some(parent_ref) = &item.parent_reference {
            if let Some(parent_graph_path) = &parent_ref.path {
                // Graph path looks like "/drive/root:/Folder/SubFolder"
                // Strip the "/drive/root:" prefix
                let rel = parent_graph_path
                    .split_once("/drive/root:")
                    .map(|(_, r)| r)
                    .unwrap_or(parent_graph_path);
                let rel = rel.trim_start_matches('/');
                let local_parent = if rel.is_empty() {
                    self.config.sync_dir.clone()
                } else {
                    self.config.sync_dir.join(rel)
                };
                return Ok(local_parent.join(&item.name));
            }
        }
        Ok(self.config.sync_dir.join(&item.name))
    }

    fn drive_item_to_db(
        &self,
        item: &DriveItem,
        local_path: &Path,
        is_placeholder: bool,
    ) -> DbItem {
        DbItem {
            id: item.id.clone(),
            local_path: local_path.to_path_buf(),
            name: item.name.clone(),
            parent_id: item.parent_reference.as_ref().and_then(|r| r.id.clone()),
            etag: item.e_tag.clone(),
            ctag: item.c_tag.clone(),
            size: item.size.unwrap_or(0),
            modified_at: item.last_modified_date_time,
            created_at: item.created_date_time,
            sha1_hash: item.sha1_hash().map(str::to_owned),
            quick_xor_hash: item.quick_xor_hash().map(str::to_owned),
            is_folder: item.is_folder(),
            is_placeholder,
            sync_state: if is_placeholder {
                SyncState::CloudOnly
            } else {
                SyncState::Synced
            },
            // pinned defaults to false for new items; upsert preserves existing value.
            pinned: false,
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        self.config
            .excluded_patterns
            .iter()
            .any(|p| name_matches_pattern(name, p))
    }
}

/// Simple glob matching: only supports leading/trailing wildcards.
/// Case-insensitive, since OneDrive itself is case-insensitive and Windows
/// artifacts vary in casing (e.g. `Thumbs.db` vs `thumbs.db`).
fn name_matches_pattern(name: &str, pattern: &str) -> bool {
    let name = name.to_lowercase();
    let pattern = pattern.to_lowercase();
    if let Some(inner) = pattern.strip_prefix('*').and_then(|p| p.strip_suffix('*')) {
        name.contains(inner)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        name.ends_with(suffix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        name == pattern
    }
}

trait PathExt {
    fn file_extension_str(&self) -> Option<&str>;
}

impl PathExt for Path {
    fn file_extension_str(&self) -> Option<&str> {
        self.extension().and_then(|e| e.to_str())
    }
}

#[cfg(test)]
mod tests {
    use super::name_matches_pattern;

    #[test]
    fn exact_match() {
        assert!(name_matches_pattern("desktop.ini", "desktop.ini"));
        assert!(!name_matches_pattern("desktop.ini.bak", "desktop.ini"));
    }

    #[test]
    fn suffix_wildcard() {
        assert!(name_matches_pattern("notes.tmp", "*.tmp"));
        assert!(!name_matches_pattern("notes.txt", "*.tmp"));
    }

    #[test]
    fn prefix_wildcard() {
        assert!(name_matches_pattern("~$report.docx", "~$*"));
        assert!(name_matches_pattern(".~lock.file.odt", ".~lock.*"));
        assert!(!name_matches_pattern("report.docx", "~$*"));
    }

    #[test]
    fn contains_wildcard() {
        assert!(name_matches_pattern("a.partial.download", "*partial*"));
        assert!(!name_matches_pattern("complete.download", "*partial*"));
    }

    #[test]
    fn case_insensitive() {
        assert!(name_matches_pattern("Thumbs.db", "thumbs.db"));
        assert!(name_matches_pattern("NOTES.TMP", "*.tmp"));
    }

    #[test]
    fn bare_star_matches_everything() {
        assert!(name_matches_pattern("anything", "*"));
    }
}
