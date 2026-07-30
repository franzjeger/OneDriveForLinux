//! Local filesystem watching and uploads (non-on-demand mode).

use super::*;

impl SyncEngine {
    pub(super) async fn local_watcher_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let sync_dir = self.config.sync_dir.clone();
        tokio::fs::create_dir_all(&sync_dir).await?;

        let mut watcher = LocalWatcher::new(&sync_dir)?;
        let mut debouncer = EventDebouncer::new();

        loop {
            // Wait up to 100ms for an event, then check debounce.
            match tokio::time::timeout(std::time::Duration::from_millis(100), watcher.events.recv())
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

    /// Upload a local path to OneDrive.
    /// Holds a per-item lock (if item exists in DB) to prevent concurrent operations.
    pub async fn upload_item(&self, path: &Path) -> anyhow::Result<()> {
        // Acquire per-item lock if this file is already tracked.
        let existing_id = self
            .db
            .get_item_by_path(path)
            .await
            .ok()
            .flatten()
            .map(|i| i.id);
        let _guard = if let Some(ref id) = existing_id {
            let lock = self.item_lock(id);
            Some(lock.lock_owned().await)
        } else {
            None
        };

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
                        if let Err(e) = self
                            .event_tx
                            .send(SyncEvent::Error(format!("Upload failed for {path:?}: {e}")))
                        {
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
        let parent_path = path.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;

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
}
