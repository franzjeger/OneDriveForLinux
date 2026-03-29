use crate::{
    config::Config,
    db::{Database, DbItem},
    state::{SyncEvent, SyncState},
    watcher::{EventDebouncer, LocalWatcher},
};
use chrono::Utc;
use graph_client::{DriveItem, GraphClient};
use std::{path::Path, path::PathBuf, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::Instant};
use tokio::sync::{broadcast, RwLock, Semaphore};
use tracing::{debug, error, info, warn};

pub struct SyncEngine {
    config: Arc<Config>,
    db: Arc<Database>,
    graph: Arc<GraphClient>,
    event_tx: broadcast::Sender<SyncEvent>,
    paused: Arc<RwLock<bool>>,
    /// Cache directory for on-demand file storage (outside the FUSE mountpoint).
    cache_dir: Option<std::path::PathBuf>,
}

impl SyncEngine {
    pub fn new(
        config: Arc<Config>,
        db: Arc<Database>,
        graph: Arc<GraphClient>,
        cache_dir: Option<std::path::PathBuf>,
    ) -> (Self, broadcast::Receiver<SyncEvent>) {
        let (event_tx, event_rx) = broadcast::channel(256);
        let engine = SyncEngine {
            config,
            db,
            graph,
            event_tx,
            paused: Arc::new(RwLock::new(false)),
            cache_dir,
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
                }).await;
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
                    let result = tokio::task::spawn(async move {
                        engine_clone.local_watcher_loop().await
                    }).await;
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

        info!("SyncEngine started");
        Ok(())
    }

    // ── Remote polling loop ────────────────────────────────────────────────────

    async fn remote_watcher_loop(self: Arc<Self>) {
        loop {
            if !self.is_paused().await {
                if let Err(e) = self.run_delta_sync().await {
                    error!("Delta sync error: {e}");
                    if let Err(e) = self.event_tx.send(SyncEvent::Error(e.to_string())) {
                        warn!("Failed to send Error event: {e}");
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(
                self.config.delta_poll_interval_secs,
            ))
            .await;
        }
    }

    async fn run_delta_sync(&self) -> anyhow::Result<()> {
        info!("Delta sync: fetching drive root");
        let root = self.graph.get_drive_root().await?;
        let folder_id = root.id.clone();
        info!("Delta sync: root id = {folder_id}");

        let delta_link = self.db.get_delta_link(&folder_id).await?;
        let was_full_sync = delta_link.is_none();

        info!("Delta sync: calling get_delta (delta_link={})", delta_link.is_some());
        let response = self
            .graph
            .get_delta(&folder_id, delta_link.as_deref())
            .await?;

        info!("Delta sync: got {} items, delta_link={}", response.items.len(), response.delta_link.is_some());

        let had_items = !response.items.is_empty();
        let seen_ids = if had_items {
            if let Err(e) = self.event_tx.send(SyncEvent::SyncStarted) {
                warn!("Failed to send SyncStarted event: {e}");
            }
            self.handle_delta(response.items).await
        } else {
            std::collections::HashSet::new()
        };

        if let Some(link) = response.delta_link {
            self.db.set_delta_link(&folder_id, &link).await?;
        }

        // After a full sync, items in sync_folders that didn't appear in the
        // delta response were deleted from OneDrive — remove them locally too.
        if was_full_sync && !self.config.sync_folders.is_empty() {
            self.reconcile_deleted_items(&seen_ids).await;
        }

        if had_items {
            if let Err(e) = self.event_tx.send(SyncEvent::SyncCompleted) {
                warn!("Failed to send SyncCompleted event: {e}");
            }
        }
        Ok(())
    }

    // ── Local watcher loop ─────────────────────────────────────────────────────

    async fn local_watcher_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let sync_dir = self.config.sync_dir.clone();
        tokio::fs::create_dir_all(&sync_dir).await?;

        let mut watcher = LocalWatcher::new(&sync_dir)?;
        let mut debouncer = EventDebouncer::new();

        loop {
            // Wait up to 100ms for an event, then check debounce.
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                watcher.events.recv(),
            )
            .await
            {
                Ok(Some(event)) => {
                    if self.is_paused().await {
                        continue;
                    }
                    let ready = debouncer.feed(event);
                    for ev in ready {
                        self.handle_local_event(ev).await;
                    }
                }
                Ok(None) => {
                    warn!("Local watcher channel closed (notify backend died?) — exiting loop");
                    break;
                }
                Err(_) => {
                    // Timeout — drain debouncer
                    let now = Instant::now();
                    let ready = debouncer.drain_ready(now);
                    for ev in ready {
                        self.handle_local_event(ev).await;
                    }
                }
            }
        }
        Ok(())
    }

    // ── Core sync operations ───────────────────────────────────────────────────

    /// Process a batch of remote changes from the delta API.
    /// Returns the set of item IDs seen (used for full-sync reconciliation).
    pub async fn handle_delta(&self, changes: Vec<DriveItem>) -> std::collections::HashSet<String> {
        let mut seen_ids = std::collections::HashSet::with_capacity(changes.len());
        // Items that only need a DB record (outside sync_folders) are collected
        // and written in a single transaction for a large performance gain.
        let mut db_batch: Vec<DbItem> = Vec::new();

        for item in changes {
            seen_ids.insert(item.id.clone());

            if item.is_deleted() {
                // Flush pending batch before any mutation that reads the DB.
                if !db_batch.is_empty() {
                    if let Err(e) = self.db.upsert_items_batch(std::mem::take(&mut db_batch)).await {
                        error!("Failed to flush DB batch before delete: {e}");
                    }
                }
                if let Ok(Some(db_item)) = self.db.get_item_by_id(&item.id).await {
                    info!("Remote delete: {:?}", db_item.local_path);
                    if let Err(e) = self.remove_local(&db_item.local_path, db_item.is_folder).await {
                        warn!("Failed to remove local file {:?}: {e}", db_item.local_path);
                    }
                    if let Err(e) = self.db.delete_item(&item.id).await {
                        warn!("Failed to delete DB item {}: {e}", item.id);
                    }
                }
                continue;
            }

            if item.is_root() || item.is_remote_item() {
                continue;
            }

            // Items outside sync_folders need only a DB record — batch them.
            if !self.config.on_demand && !self.config.sync_folders.is_empty() {
                if let Ok(local_path) = self.item_local_path(&item) {
                    let in_sync_folder = self.config.sync_folders.iter().any(|folder| {
                        local_path.starts_with(self.config.sync_dir.join(folder))
                    });
                    if !in_sync_folder {
                        db_batch.push(self.drive_item_to_db(&item, &local_path, true));
                        continue;
                    }
                }
            }

            // Items in sync_folders (or on_demand mode) need real file I/O.
            // Flush the batch first so DB is consistent for reads inside sync_item.
            if !db_batch.is_empty() {
                if let Err(e) = self.db.upsert_items_batch(std::mem::take(&mut db_batch)).await {
                    error!("Failed to flush DB batch: {e}");
                }
            }
            match self.sync_item(&item).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to sync item {}: {e}", item.id);
                    if let Err(e2) = self.db.set_sync_state(
                        &item.id,
                        &SyncState::Error(e.to_string()),
                    ).await {
                        warn!("Failed to set error state for {}: {e2}", item.id);
                    }
                }
            }
        }

        // Flush any remaining DB-only items.
        if !db_batch.is_empty() {
            if let Err(e) = self.db.upsert_items_batch(db_batch).await {
                error!("Failed to flush final DB batch: {e}");
            }
        }

        seen_ids
    }

    /// Sync a remote DriveItem to local disk (or create a placeholder).
    pub async fn sync_item(&self, item: &DriveItem) -> anyhow::Result<()> {
        // The root drive item maps to sync_dir itself — skip it.
        if item.is_root() {
            return Ok(());
        }

        // Remote items live in a different drive (e.g. Teams Chat Files shortcuts
        // to SharePoint/Teams). They can't be downloaded via the personal OneDrive
        // API, so skip them entirely.
        if item.is_remote_item() {
            debug!("Skipping remote item: {} ({})", item.name, item.id);
            return Ok(());
        }

        let local_path = self.item_local_path(item)?;

        // sync_folders filter: when set, only download items whose local path is
        // under one of the configured top-level folders. Items outside are stored
        // in the DB as cloud-only so they appear in the FUSE mount but are never
        // written to disk. Only applies in non-on_demand mode.
        if !self.config.on_demand && !self.config.sync_folders.is_empty() {
            let in_sync_folder = self.config.sync_folders.iter().any(|folder| {
                local_path.starts_with(self.config.sync_dir.join(folder))
            });
            if !in_sync_folder {
                // Record in DB but do not download.
                let db_item = self.drive_item_to_db(item, &local_path, true);
                self.db.upsert_item(&db_item).await?;
                return Ok(());
            }
        }

        if item.is_folder() {
            // In on_demand mode the FUSE fs is mounted at sync_dir — don't write
            // through it. Just record the folder in the DB; FUSE will serve it.
            if !self.config.on_demand {
                info!("Creating directory: {:?}", local_path);
                tokio::fs::create_dir_all(&local_path).await?;
            }
            let db_item = self.drive_item_to_db(item, &local_path, false);
            self.db.upsert_item(&db_item).await?;
            if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
                path: local_path,
                state: SyncState::Synced,
            }) {
                warn!("Failed to send ItemStateChanged event: {e}");
            }
            return Ok(());
        }

        // Check for conflict: local file modified more recently than remote.
        // Skip in on_demand mode — local_path is a FUSE path.
        if !self.config.on_demand && local_path.exists() {
            // If the etag matches what we have in the DB the remote hasn't changed
            // since our last sync — no conflict possible, skip the check entirely.
            let existing = self.db.get_item_by_id(&item.id).await?;
            let etag_unchanged = existing.as_ref()
                .and_then(|e| e.etag.as_deref())
                .zip(item.e_tag.as_deref())
                .map(|(db_etag, remote_etag)| db_etag == remote_etag)
                .unwrap_or(false);

            if !etag_unchanged {
                let local_meta = tokio::fs::metadata(&local_path).await?;
                let local_mtime = local_meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                let remote_mtime = item
                    .last_modified_date_time
                    .map(|d| d.timestamp() as u64);

                if let (Some(lm), Some(rm)) = (local_mtime, remote_mtime) {
                    if lm > rm && existing.is_some() {
                        self.reconcile_conflict(&local_path, item).await?;
                        return Ok(());
                    }
                }
            }
        }

        if self.config.on_demand {
            // Check if this item is already pinned in the DB — if so, keep it local.
            let is_pinned = self.db.get_item_by_id(&item.id).await
                .ok().flatten().map(|i| i.pinned).unwrap_or(false);

            if is_pinned {
                // Ensure it's downloaded to cache.
                if let Some(cache_dir) = &self.cache_dir {
                    let cache_path = cache_dir.join(&item.id);
                    if !cache_path.exists() {
                        if let Err(e) = self.graph.download_file(&item.id, &cache_path).await {
                            warn!("Pinned item {} download failed: {e}", item.id);
                        }
                    }
                }
                let db_item = self.drive_item_to_db(item, &local_path, false);
                self.db.upsert_item(&db_item).await?;
                if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
                    path: local_path,
                    state: SyncState::Pinned,
                }) {
                    warn!("Failed to send ItemStateChanged event: {e}");
                }
            } else {
                // Not pinned: create a placeholder — the VFS layer will fetch on access.
                let db_item = self.drive_item_to_db(item, &local_path, true);
                self.db.upsert_item(&db_item).await?;
                if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
                    path: local_path,
                    state: SyncState::CloudOnly,
                }) {
                    warn!("Failed to send ItemStateChanged event: {e}");
                }
            }
        } else {
            // Skip download if the file already exists locally and the remote
            // etag hasn't changed since we last synced it.
            let existing = self.db.get_item_by_id(&item.id).await?;
            let etag_unchanged = existing.as_ref()
                .and_then(|e| e.etag.as_deref())
                .zip(item.e_tag.as_deref())
                .map(|(db_etag, remote_etag)| db_etag == remote_etag)
                .unwrap_or(false);

            if etag_unchanged && local_path.exists() {
                // File is up to date — just ensure DB reflects current state.
                let db_item = self.drive_item_to_db(item, &local_path, false);
                self.db.upsert_item(&db_item).await?;
            } else {
                self.download_item(item, &local_path).await?;
            }
        }

        Ok(())
    }

    /// Download a remote item to local disk and update the database.
    pub async fn download_item(&self, item: &DriveItem, local_path: &Path) -> anyhow::Result<()> {
        if let Err(e) = self.db.set_sync_state(&item.id, &SyncState::Syncing).await {
            warn!("Failed to set Syncing state for {}: {e}", item.id);
        }
        if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
            path: local_path.to_path_buf(),
            state: SyncState::Syncing,
        }) {
            warn!("Failed to send ItemStateChanged event: {e}");
        }

        self.graph.download_file(&item.id, local_path).await?;

        let db_item = self.drive_item_to_db(item, local_path, false);
        self.db.upsert_item(&db_item).await?;

        if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
            path: local_path.to_path_buf(),
            state: SyncState::Synced,
        }) {
            warn!("Failed to send ItemStateChanged event: {e}");
        }
        info!("Downloaded {:?}", local_path);
        Ok(())
    }

    /// Upload a local path to OneDrive.
    pub async fn upload_item(&self, path: &Path) -> anyhow::Result<()> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid file name: {path:?}"))?;

        // Determine parent item ID
        let parent_path = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("no parent for {path:?}"))?;

        let parent_id = if let Some(db_item) = self.db.get_item_by_path(parent_path).await? {
            db_item.id
        } else {
            // Fall back to drive root
            let root = self.graph.get_drive_root().await?;
            root.id
        };

        if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
            path: path.to_path_buf(),
            state: SyncState::Syncing,
        }) {
            warn!("Failed to send ItemStateChanged event: {e}");
        }

        let result_item = self.graph.upload_file(&parent_id, name, path).await?;
        let db_item = self.drive_item_to_db(&result_item, path, false);
        self.db.upsert_item(&db_item).await?;

        if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
            path: path.to_path_buf(),
            state: SyncState::Synced,
        }) {
            warn!("Failed to send ItemStateChanged event: {e}");
        }
        info!("Uploaded {:?}", path);
        Ok(())
    }

    /// Handle a conflict: rename local with timestamp, then download remote.
    pub async fn reconcile_conflict(
        &self,
        local_path: &Path,
        remote: &DriveItem,
    ) -> anyhow::Result<()> {
        let stem = local_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("conflict");
        let ext = local_path
            .file_extension_str()
            .unwrap_or_default();
        let ts = Utc::now().format("%Y%m%d_%H%M%S");
        let conflict_name = if ext.is_empty() {
            format!("{stem}_conflict_{ts}")
        } else {
            format!("{stem}_conflict_{ts}.{ext}")
        };
        let conflict_path = local_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&conflict_name);

        warn!(
            "Conflict for {:?} — renaming local to {:?}",
            local_path, conflict_path
        );
        tokio::fs::rename(local_path, &conflict_path).await?;

        // Download the remote version
        self.download_item(remote, local_path).await?;
        Ok(())
    }

    /// Handle a local filesystem event (create/modify/delete/rename).
    pub async fn handle_local_event(&self, event: notify::Event) {
        if self.is_paused().await {
            return;
        }

        for path in &event.paths {
            if self.is_excluded(path) {
                continue;
            }

            if EventDebouncer::is_create_or_modify(&event.kind) {
                if path.is_file() {
                    info!("Local change detected: {:?}", path);
                    if let Err(e) = self.upload_item(path).await {
                        error!("Upload failed for {:?}: {e}", path);
                        if let Err(e) = self.event_tx.send(SyncEvent::Error(format!(
                            "Upload failed for {path:?}: {e}"
                        ))) {
                            warn!("Failed to send Error event: {e}");
                        }
                    }
                } else if path.is_dir() {
                    if let Err(e) = self.upload_directory(path).await {
                        error!("Folder create failed for {:?}: {e}", path);
                    }
                }
            } else if EventDebouncer::is_remove(&event.kind) {
                debug!("Local delete: {:?}", path);
                if let Ok(Some(db_item)) = self.db.get_item_by_path(path).await {
                    match self.graph.delete_item(&db_item.id).await {
                        Ok(_) => {
                            if let Err(e) = self.db.delete_item(&db_item.id).await {
                                warn!("Failed to delete DB item {}: {e}", db_item.id);
                            }
                        }
                        Err(e) => {
                            error!("Remote delete failed for {}: {e}", db_item.id);
                        }
                    }
                }
            }
        }
    }

    async fn upload_directory(&self, path: &Path) -> anyhow::Result<()> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid dir name"))?;
        let parent_path = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("no parent"))?;

        let parent_id = if let Some(db_item) = self.db.get_item_by_path(parent_path).await? {
            db_item.id
        } else {
            self.graph.get_drive_root().await?.id
        };

        let folder_item = self.graph.create_folder(&parent_id, name).await?;
        let db_item = self.drive_item_to_db(&folder_item, path, false);
        self.db.upsert_item(&db_item).await?;
        Ok(())
    }

    async fn remove_local(&self, path: &Path, is_folder: bool) -> anyhow::Result<()> {
        // In on_demand mode the path is inside the FUSE mount — accessing it from
        // within the daemon causes a recursive FUSE deadlock. The FUSE filesystem
        // reflects DB state, so removing from the DB is sufficient; the entry will
        // vanish from the mount automatically. Only touch the real FS in normal mode.
        if self.config.on_demand {
            return Ok(());
        }
        if path.exists() {
            if is_folder {
                tokio::fs::remove_dir_all(path).await?;
            } else {
                tokio::fs::remove_file(path).await?;
            }
        }
        Ok(())
    }

    /// After a full delta sync, find items in our DB that were absent from the
    /// delta response — they were deleted from OneDrive.
    async fn reconcile_deleted_items(&self, seen_ids: &std::collections::HashSet<String>) {
        let all_db_items = match self.db.all_items().await {
            Ok(items) => items,
            Err(e) => {
                warn!("reconcile_deleted_items: DB query failed: {e}");
                return;
            }
        };

        for db_item in all_db_items {
            if seen_ids.contains(&db_item.id) {
                continue;
            }
            // Item no longer exists on OneDrive.
            if !db_item.is_placeholder {
                info!("Reconcile: remote-deleted {:?}", db_item.local_path);
                if let Err(e) = self.remove_local(&db_item.local_path, db_item.is_folder).await {
                    warn!("Reconcile: failed to remove local {:?}: {e}", db_item.local_path);
                }
            }
            if let Err(e) = self.db.delete_item(&db_item.id).await {
                warn!("Reconcile: failed to delete DB item {}: {e}", db_item.id);
            }
        }
    }

    // ── Pin / unpin ────────────────────────────────────────────────────────────

    /// Pin a file or folder — mark it always-on-device and download immediately.
    /// For folders, all files within are pinned and downloaded recursively.
    pub async fn pin_item(self: Arc<Self>, path: &Path) -> anyhow::Result<()> {
        let n = self.db.set_pinned_for_path(path, true).await?;
        if n == 0 {
            anyhow::bail!("Path not found in OneDrive: {:?}", path);
        }

        let cache_dir = self.cache_dir.clone()
            .ok_or_else(|| anyhow::anyhow!("on_demand mode not active"))?;

        // Collect files that need downloading: cloud-only or placeholder.
        let to_download: Vec<_> = {
            let item = self.db.get_item_by_path(path).await?;
            if item.as_ref().map(|i| i.is_folder).unwrap_or(false) {
                self.db.get_files_under(path).await?
                    .into_iter()
                    .filter(|i| i.is_placeholder || !matches!(i.sync_state, SyncState::Synced | SyncState::Pinned))
                    .collect()
            } else {
                item.into_iter()
                    .filter(|i| i.is_placeholder || !matches!(i.sync_state, SyncState::Synced | SyncState::Pinned))
                    .collect()
            }
        };

        let total = to_download.len();
        info!("Pinning {:?}: downloading {total} file(s)", path);

        if total == 0 {
            return Ok(());
        }

        // Tray: show spinning icon while downloads are in progress.
        if let Err(e) = self.event_tx.send(SyncEvent::SyncStarted) {
            warn!("Failed to send SyncStarted event: {e}");
        }

        // Mark every file as Syncing immediately so overlays update at once.
        for db_item in &to_download {
            if let Err(e) = self.db.set_sync_state(&db_item.id, &SyncState::Syncing).await {
                warn!("Failed to set Syncing state for {}: {e}", db_item.id);
            }
            if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
                path: db_item.local_path.clone(),
                state: SyncState::Syncing,
            }) {
                warn!("Failed to send ItemStateChanged event: {e}");
            }
        }

        // Counter: last download to finish sends SyncCompleted.
        let remaining = Arc::new(AtomicUsize::new(total));

        // Spawn background downloads so the D-Bus call returns immediately.
        // Limit concurrency to avoid Graph API rate limiting.
        let sem = Arc::new(Semaphore::new(4));
        for db_item in to_download {
            let graph = Arc::clone(&self.graph);
            let db = Arc::clone(&self.db);
            let tx = self.event_tx.clone();
            let cache_dir = cache_dir.clone();
            let item_local_path = db_item.local_path.clone();
            let id = db_item.id.clone();
            let remaining = Arc::clone(&remaining);
            let sem = Arc::clone(&sem);

            tokio::spawn(async move {
                let _permit = sem.acquire().await;
                let cache_path = cache_dir.join(&id);
                if !cache_path.exists() {
                    match graph.download_file(&id, &cache_path).await {
                        Ok(_) => {
                            if let Err(e) = db.set_placeholder(&id, false).await {
                                warn!("Pin: failed to clear placeholder for {id}: {e}");
                            }
                            if let Err(e) = db.set_sync_state(&id, &SyncState::Pinned).await {
                                warn!("Pin: failed to set Pinned state for {id}: {e}");
                            }
                        }
                        Err(e) => {
                            error!("Pin download failed for {id}: {e}");
                            if let Err(e2) = db.set_sync_state(&id, &SyncState::Error(e.to_string())).await {
                                warn!("Pin: failed to set Error state for {id}: {e2}");
                            }
                        }
                    }
                } else {
                    if let Err(e) = db.set_placeholder(&id, false).await {
                        warn!("Pin: failed to clear placeholder for {id}: {e}");
                    }
                    if let Err(e) = db.set_sync_state(&id, &SyncState::Pinned).await {
                        warn!("Pin: failed to set Pinned state for {id}: {e}");
                    }
                }
                if let Err(e) = tx.send(SyncEvent::ItemStateChanged {
                    path: item_local_path,
                    state: SyncState::Pinned,
                }) {
                    warn!("Pin: failed to send ItemStateChanged event: {e}");
                }
                // Last download signals completion so tray returns to idle.
                if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                    if let Err(e) = tx.send(SyncEvent::SyncCompleted) {
                        warn!("Pin: failed to send SyncCompleted event: {e}");
                    }
                }
            });
        }

        Ok(())
    }

    /// Unpin a file or folder — remove it from cache and convert back to cloud-only placeholder.
    /// For folders, all files within are unpinned recursively.
    pub async fn unpin_item(&self, path: &Path) -> anyhow::Result<()> {
        let n = self.db.set_pinned_for_path(path, false).await?;
        if n == 0 {
            anyhow::bail!("Path not found in OneDrive: {:?}", path);
        }

        let cache_dir = self.cache_dir.as_ref()
            .ok_or_else(|| anyhow::anyhow!("on_demand mode not active"))?;

        let to_free: Vec<_> = {
            let item = self.db.get_item_by_path(path).await?;
            if item.as_ref().map(|i| i.is_folder).unwrap_or(false) {
                self.db.get_files_under(path).await?
            } else {
                item.into_iter().collect()
            }
        };

        for db_item in to_free {
            // Remove cached file to free disk space.
            let cache_path = cache_dir.join(&db_item.id);
            if cache_path.exists() {
                if let Err(e) = std::fs::remove_file(&cache_path) {
                    warn!("Could not remove cache for {}: {e}", db_item.id);
                }
            }
            if let Err(e) = self.db.set_placeholder(&db_item.id, true).await {
                warn!("Unpin: failed to set placeholder for {}: {e}", db_item.id);
            }
            if let Err(e) = self.db.set_sync_state(&db_item.id, &SyncState::CloudOnly).await {
                warn!("Unpin: failed to set CloudOnly state for {}: {e}", db_item.id);
            }
            if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
                path: db_item.local_path,
                state: SyncState::CloudOnly,
            }) {
                warn!("Unpin: failed to send ItemStateChanged event: {e}");
            }
        }

        info!("Unpinned {:?}", path);
        Ok(())
    }

    // ── Status ─────────────────────────────────────────────────────────────────

    pub async fn get_status(&self) -> Vec<(PathBuf, SyncState)> {
        self.db
            .all_items()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|i| (i.local_path, i.sync_state))
            .collect()
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

    fn drive_item_to_db(&self, item: &DriveItem, local_path: &Path, is_placeholder: bool) -> DbItem {
        DbItem {
            id: item.id.clone(),
            local_path: local_path.to_path_buf(),
            name: item.name.clone(),
            parent_id: item
                .parent_reference
                .as_ref()
                .and_then(|r| r.id.clone()),
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
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        for pattern in &self.config.excluded_patterns {
            // Simple glob matching: only supports leading/trailing wildcards
            if pattern.starts_with('*') && pattern.ends_with('*') {
                let inner = &pattern[1..pattern.len() - 1];
                if name.contains(inner) {
                    return true;
                }
            } else if let Some(suffix) = pattern.strip_prefix('*') {
                if name.ends_with(suffix) {
                    return true;
                }
            } else if let Some(prefix) = pattern.strip_suffix('*') {
                if name.starts_with(prefix) {
                    return true;
                }
            } else if name == pattern.as_str() {
                return true;
            }
        }
        false
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
