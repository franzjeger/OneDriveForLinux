//! Remote delta sync: polling, item download, reconciliation, cache cleanup.

use super::*;

impl SyncEngine {
    /// One delta fetch, reporting page progress on the event channel.
    async fn fetch_delta(
        &self,
        folder_id: &str,
        delta_link: Option<&str>,
    ) -> Result<DeltaResponse, GraphError> {
        let tx = self.event_tx.clone();
        self.graph
            .get_delta_with_progress(folder_id, delta_link, move |_page, items| {
                let _ = tx.send(SyncEvent::SyncProgress(format!(
                    "Fetching file list… {items} items"
                )));
            })
            .await
    }

    pub(super) async fn run_delta_sync(&self) -> anyhow::Result<()> {
        info!("Delta sync: fetching drive root");
        let root = self.graph.get_drive_root().await?;
        let folder_id = root.id.clone();
        info!("Delta sync: root id = {folder_id}");

        let delta_link = self.db.get_delta_link(&folder_id).await?;
        let mut was_full_sync = delta_link.is_none();

        if was_full_sync {
            info!("Delta sync: first run — fetching the full file list, this can take a while");
        }
        // Announce before the fetch: on a large drive the initial delta takes
        // minutes, and the tray must not claim "up to date" meanwhile.
        if let Err(e) = self.event_tx.send(SyncEvent::SyncStarted) {
            warn!("Failed to send SyncStarted event: {e}");
        }

        let mut response = self.fetch_delta(&folder_id, delta_link.as_deref()).await;

        // Graph expires delta tokens (a mailbox move, a long gap, a service-side
        // change) and answers 410 resyncRequired. The stored token is dead, so
        // retrying it would fail identically forever — drop it and start over.
        if matches!(response, Err(GraphError::ResyncRequired)) {
            warn!("Delta token rejected by Graph (resyncRequired) — starting a full resync");
            let _ = self.event_tx.send(SyncEvent::SyncProgress(
                "Rebuilding the file list…".to_string(),
            ));
            if let Err(e) = self.db.clear_delta_link(&folder_id).await {
                warn!("Failed to clear the stale delta link: {e}");
            }
            was_full_sync = true;
            response = self.fetch_delta(&folder_id, None).await;
        }
        let response = response?;

        info!(
            "Delta sync: got {} items, delta_link={}",
            response.items.len(),
            response.delta_link.is_some()
        );

        let had_items = !response.items.is_empty();
        let seen_ids = if had_items {
            let _ = self.event_tx.send(SyncEvent::SyncProgress(format!(
                "Processing {} changes…",
                response.items.len()
            )));
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

        if let Err(e) = self.event_tx.send(SyncEvent::SyncCompleted) {
            warn!("Failed to send SyncCompleted event: {e}");
        }

        // Periodic cleanup: remove stale cache files and tmp leftovers.
        self.cleanup_cache().await;

        Ok(())
    }

    // ── Local watcher loop ─────────────────────────────────────────────────────

    /// Process a batch of remote changes from the delta API.
    /// Returns the set of item IDs seen (used for full-sync reconciliation).
    pub async fn handle_delta(&self, changes: Vec<DriveItem>) -> std::collections::HashSet<String> {
        let mut seen_ids = std::collections::HashSet::with_capacity(changes.len());
        // Items that only need a DB record (outside sync_folders, or on-demand
        // placeholders) are collected and written in a single transaction. Doing
        // this one-by-one costs an fsync per item, which dominates a first sync.
        let mut db_batch: Vec<DbItem> = Vec::new();

        // In on-demand mode almost every item is a placeholder; the only reason
        // to touch the DB per item is to check whether it's pinned. Pinned items
        // are rare, so load their IDs once and batch everything else.
        let pinned = if self.config.on_demand {
            match self.db.pinned_ids().await {
                Ok(ids) => Some(ids),
                Err(e) => {
                    warn!("Failed to load pinned IDs, falling back to per-item sync: {e}");
                    None
                }
            }
        } else {
            None
        };

        for item in changes {
            seen_ids.insert(item.id.clone());

            if item.is_deleted() {
                // Flush pending batch before any mutation that reads the DB.
                if !db_batch.is_empty() {
                    if let Err(e) = self
                        .db
                        .upsert_items_batch(std::mem::take(&mut db_batch))
                        .await
                    {
                        error!("Failed to flush DB batch before delete: {e}");
                    }
                }
                if let Ok(Some(db_item)) = self.db.get_item_by_id(&item.id).await {
                    info!("Remote delete: {:?}", db_item.local_path);
                    if let Err(e) = self
                        .remove_local(&db_item.local_path, db_item.is_folder)
                        .await
                    {
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

            // On-demand, unpinned items are pure metadata: a folder row or a
            // cloud-only placeholder. Neither touches the filesystem, so they
            // can go straight into the batch.
            if let Some(pinned) = &pinned {
                if !pinned.contains(&item.id) {
                    if let Ok(local_path) = self.item_local_path(&item) {
                        let is_folder = item.is_folder();
                        db_batch.push(self.drive_item_to_db(&item, &local_path, !is_folder));
                        let state = if is_folder {
                            SyncState::Synced
                        } else {
                            SyncState::CloudOnly
                        };
                        let _ = self.event_tx.send(SyncEvent::ItemStateChanged {
                            path: local_path,
                            state,
                        });
                        continue;
                    }
                }
            }

            // Items outside sync_folders need only a DB record — batch them.
            if !self.config.on_demand && !self.config.sync_folders.is_empty() {
                if let Ok(local_path) = self.item_local_path(&item) {
                    let in_sync_folder =
                        self.config.sync_folders.iter().any(|folder| {
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
                if let Err(e) = self
                    .db
                    .upsert_items_batch(std::mem::take(&mut db_batch))
                    .await
                {
                    error!("Failed to flush DB batch: {e}");
                }
            }
            match self.sync_item(&item).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to sync item {}: {e}", item.id);
                    if let Err(e2) = self
                        .db
                        .set_sync_state(&item.id, &SyncState::Error(e.to_string()))
                        .await
                    {
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
            let in_sync_folder = self
                .config
                .sync_folders
                .iter()
                .any(|folder| local_path.starts_with(self.config.sync_dir.join(folder)));
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
            let etag_unchanged = existing
                .as_ref()
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
                let remote_mtime = item.last_modified_date_time.map(|d| d.timestamp() as u64);

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
            let is_pinned = self
                .db
                .get_item_by_id(&item.id)
                .await
                .ok()
                .flatten()
                .map(|i| i.pinned)
                .unwrap_or(false);

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
            let etag_unchanged = existing
                .as_ref()
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
    /// Holds a per-item lock to prevent concurrent downloads of the same file.
    pub async fn download_item(&self, item: &DriveItem, local_path: &Path) -> anyhow::Result<()> {
        let lock = self.item_lock(&item.id);
        let _guard = lock.lock().await;

        Self::check_disk_space(local_path)?;

        if let Err(e) = self.db.set_sync_state(&item.id, &SyncState::Syncing).await {
            warn!("Failed to set Syncing state for {}: {e}", item.id);
        }
        if let Err(e) = self.event_tx.send(SyncEvent::ItemStateChanged {
            path: local_path.to_path_buf(),
            state: SyncState::Syncing,
        }) {
            warn!("Failed to send ItemStateChanged event: {e}");
        }

        if let Err(e) = self.graph.download_file(&item.id, local_path).await {
            // Reset the Syncing state so the item isn't stuck as "Syncing" forever.
            if let Err(e2) = self
                .db
                .set_sync_state(&item.id, &SyncState::Error(e.to_string()))
                .await
            {
                warn!("Failed to set Error state for {}: {e2}", item.id);
            }
            return Err(e.into());
        }

        // Integrity check against the server-reported QuickXorHash. Warn-only
        // for now: our implementation is validated by unit tests but not yet
        // against Microsoft's reference in the field, so a mismatch is logged
        // loudly rather than failing the download.
        if let Some(expected) = item.quick_xor_hash() {
            let expected = expected.to_string();
            let path = local_path.to_path_buf();
            match tokio::task::spawn_blocking(move || {
                crate::quickxor::QuickXorHash::hash_file(&path)
            })
            .await
            {
                Ok(Ok(actual)) if actual != expected => {
                    warn!(
                        "QuickXorHash mismatch for {:?}: server={expected} local={actual} — file may be corrupt",
                        local_path
                    );
                }
                Ok(Ok(_)) => debug!("QuickXorHash verified for {:?}", local_path),
                Ok(Err(e)) => warn!("QuickXorHash read failed for {:?}: {e}", local_path),
                Err(e) => warn!("QuickXorHash task failed for {:?}: {e}", local_path),
            }
        }

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
        let ext = local_path.file_extension_str().unwrap_or_default();
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
            // Local-only items (created via FUSE, upload still pending) have
            // never been on OneDrive, so they can never appear in a delta
            // response — deleting them here would destroy unsynced user data.
            if db_item.id.starts_with("_local_") {
                continue;
            }
            // Item no longer exists on OneDrive.
            if !db_item.is_placeholder {
                info!("Reconcile: remote-deleted {:?}", db_item.local_path);
                if let Err(e) = self
                    .remove_local(&db_item.local_path, db_item.is_folder)
                    .await
                {
                    warn!(
                        "Reconcile: failed to remove local {:?}: {e}",
                        db_item.local_path
                    );
                }
            }
            if let Err(e) = self.db.delete_item(&db_item.id).await {
                warn!("Reconcile: failed to delete DB item {}: {e}", db_item.id);
            }
        }
    }

    /// Remove stale cache files that no longer have a corresponding DB entry,
    /// and clean up leftover .tmp files from interrupted downloads.
    /// Also prunes the item_locks DashMap to prevent unbounded growth.
    pub async fn cleanup_cache(&self) {
        let cache_dir = match &self.cache_dir {
            Some(d) => d,
            None => return,
        };

        let dir = match std::fs::read_dir(cache_dir) {
            Ok(d) => d,
            Err(_) => return,
        };

        let mut removed = 0u64;
        let mut removed_bytes = 0u64;

        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Remove leftover .tmp files from interrupted atomic downloads.
            if name.ends_with(".tmp") {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!("Cache cleanup: failed to remove tmp file {name}: {e}");
                } else {
                    removed += 1;
                    removed_bytes += size;
                }
                continue;
            }

            // Check if this cache file has a matching DB item.
            // The file name is the OneDrive item ID.
            match self.db.get_item_by_id(&name).await {
                Ok(Some(_)) => {} // Still valid
                Ok(None) => {
                    // No DB entry — stale cache file.
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if let Err(e) = std::fs::remove_file(&path) {
                        warn!("Cache cleanup: failed to remove stale {name}: {e}");
                    } else {
                        removed += 1;
                        removed_bytes += size;
                    }
                }
                Err(_) => {} // DB error — skip, don't delete
            }
        }

        // Prune item_locks: remove entries where we're the only holder of the Arc.
        self.item_locks.retain(|_, v| Arc::strong_count(v) > 1);

        if removed > 0 {
            let mb = removed_bytes as f64 / (1024.0 * 1024.0);
            info!("Cache cleanup: removed {removed} stale files ({mb:.1} MB)");
        }
    }

    // ── Status ─────────────────────────────────────────────────────────────────
}

/// Integration tests for delta handling. These exercise the paths that never
/// touch the network in on-demand mode: placeholder upserts, deletions, root
/// and remote-item skipping, and reconciliation.
#[cfg(test)]
mod delta_tests {
    use super::*;
    use crate::config::Config;

    fn test_engine(sync_dir: PathBuf) -> SyncEngine {
        let config = Arc::new(Config {
            sync_dir,
            client_id: "test-client".into(),
            tenant_id: "common".into(),
            excluded_patterns: vec![],
            sync_folders: vec![],
            on_demand: true,
            max_upload_threads: 1,
            max_download_threads: 1,
            delta_poll_interval_secs: 30,
            auth_method: "device_code".into(),
        });
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("test.db")).unwrap());
        // Leak the tempdir so the DB file outlives this constructor.
        std::mem::forget(dir);
        let auth = Arc::new(AuthManager::new("test-client".into(), "common".into()).unwrap());
        let graph = Arc::new(GraphClient::new(Arc::clone(&auth)));
        let (engine, _rx) = SyncEngine::new(config, db, graph, auth, None);
        engine
    }

    fn drive_item(json: serde_json::Value) -> DriveItem {
        serde_json::from_value(json).unwrap()
    }

    #[tokio::test]
    async fn new_remote_file_becomes_cloud_only_placeholder() {
        let sync_dir = tempfile::tempdir().unwrap();
        let engine = test_engine(sync_dir.path().to_path_buf());

        let item = drive_item(serde_json::json!({
            "id": "item1",
            "name": "doc.txt",
            "eTag": "e1",
            "size": 10,
            "file": {},
            "parentReference": {"id": "root-id", "path": "/drive/root:"}
        }));
        engine.handle_delta(vec![item]).await;

        let db_item = engine.db.get_item_by_id("item1").await.unwrap().unwrap();
        assert!(db_item.is_placeholder);
        assert_eq!(db_item.sync_state, SyncState::CloudOnly);
        assert_eq!(db_item.local_path, sync_dir.path().join("doc.txt"));
    }

    #[tokio::test]
    async fn remote_delete_removes_db_entry() {
        let sync_dir = tempfile::tempdir().unwrap();
        let engine = test_engine(sync_dir.path().to_path_buf());

        let item = drive_item(serde_json::json!({
            "id": "item1",
            "name": "doc.txt",
            "file": {},
            "parentReference": {"id": "root-id", "path": "/drive/root:"}
        }));
        engine.handle_delta(vec![item]).await;
        assert!(engine.db.get_item_by_id("item1").await.unwrap().is_some());

        let deleted = drive_item(serde_json::json!({
            "id": "item1",
            "deleted": {}
        }));
        engine.handle_delta(vec![deleted]).await;
        assert!(engine.db.get_item_by_id("item1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn root_and_remote_items_are_skipped() {
        let sync_dir = tempfile::tempdir().unwrap();
        let engine = test_engine(sync_dir.path().to_path_buf());

        let root = drive_item(serde_json::json!({
            "id": "root-id",
            "name": "root",
            "folder": {},
            "parentReference": {}
        }));
        let remote = drive_item(serde_json::json!({
            "id": "remote1",
            "name": "Teams Chat Files",
            "remoteItem": {"id": "other-drive-item"}
        }));
        let seen = engine.handle_delta(vec![root, remote]).await;

        // Both are counted as seen (so reconciliation won't delete them)...
        assert!(seen.contains("root-id"));
        assert!(seen.contains("remote1"));
        // ...but neither gets a DB row.
        assert!(engine.db.get_item_by_id("root-id").await.unwrap().is_none());
        assert!(engine.db.get_item_by_id("remote1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn folder_delta_is_recorded() {
        let sync_dir = tempfile::tempdir().unwrap();
        let engine = test_engine(sync_dir.path().to_path_buf());

        let folder = drive_item(serde_json::json!({
            "id": "folder1",
            "name": "Projects",
            "folder": {"childCount": 0},
            "parentReference": {"id": "root-id", "path": "/drive/root:"}
        }));
        engine.handle_delta(vec![folder]).await;

        let db_item = engine.db.get_item_by_id("folder1").await.unwrap().unwrap();
        assert!(db_item.is_folder);
        assert_eq!(db_item.sync_state, SyncState::Synced);
    }

    #[tokio::test]
    async fn reconciliation_spares_pending_local_items() {
        let sync_dir = tempfile::tempdir().unwrap();
        let engine = test_engine(sync_dir.path().to_path_buf());

        // A synced remote item that has vanished from the delta response...
        let stale = DbItem {
            id: "stale-remote".into(),
            local_path: sync_dir.path().join("gone.txt"),
            name: "gone.txt".into(),
            parent_id: Some("root-id".into()),
            etag: None,
            ctag: None,
            size: 1,
            modified_at: None,
            created_at: None,
            sha1_hash: None,
            quick_xor_hash: None,
            is_folder: false,
            is_placeholder: false,
            sync_state: SyncState::Synced,
            pinned: false,
        };
        // ...and a locally-created item whose upload is still pending.
        let local_only = DbItem {
            id: "_local_42".into(),
            local_path: sync_dir.path().join("new.txt"),
            name: "new.txt".into(),
            sync_state: SyncState::Syncing,
            ..stale.clone()
        };
        engine.db.upsert_item(&stale).await.unwrap();
        engine.db.upsert_item(&local_only).await.unwrap();

        engine
            .reconcile_deleted_items(&std::collections::HashSet::new())
            .await;

        // The stale remote item is gone; the pending local item survives.
        assert!(engine
            .db
            .get_item_by_id("stale-remote")
            .await
            .unwrap()
            .is_none());
        assert!(engine
            .db
            .get_item_by_id("_local_42")
            .await
            .unwrap()
            .is_some());
    }
}
