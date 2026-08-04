//! Pin / unpin: always-on-device file handling.

use super::*;

impl SyncEngine {
    /// Pin a file or folder — mark it always-on-device and download immediately.
    /// For folders, all files within are pinned and downloaded recursively.
    pub async fn pin_item(self: Arc<Self>, path: &Path) -> anyhow::Result<()> {
        let n = self.db.set_pinned_for_path(path, true).await?;
        if n == 0 {
            anyhow::bail!("Path not found in OneDrive: {:?}", path);
        }

        let cache_dir = self
            .cache_dir
            .clone()
            .ok_or_else(|| anyhow::anyhow!("on_demand mode not active"))?;

        // Collect files that need downloading: cloud-only or placeholder.
        let to_download: Vec<_> = {
            let item = self.db.get_item_by_path(path).await?;
            if item.as_ref().map(|i| i.is_folder).unwrap_or(false) {
                self.db
                    .get_files_under(path)
                    .await?
                    .into_iter()
                    .filter(|i| {
                        i.is_placeholder
                            || !matches!(i.sync_state, SyncState::Synced | SyncState::Pinned)
                    })
                    .collect()
            } else {
                item.into_iter()
                    .filter(|i| {
                        i.is_placeholder
                            || !matches!(i.sync_state, SyncState::Synced | SyncState::Pinned)
                    })
                    .collect()
            }
        };

        let total = to_download.len();
        info!("Pinning {:?}: downloading {total} file(s)", path);

        if total == 0 {
            return Ok(());
        }

        Self::check_disk_space(&cache_dir)?;

        // Tray: show spinning icon while downloads are in progress.
        if let Err(e) = self.event_tx.send(SyncEvent::SyncStarted) {
            warn!("Failed to send SyncStarted event: {e}");
        }

        // Mark every file as Syncing immediately so overlays update at once.
        for db_item in &to_download {
            if let Err(e) = self
                .db
                .set_sync_state(&db_item.id, &SyncState::Syncing)
                .await
            {
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
        let sem = Arc::new(Semaphore::new(self.config.max_download_threads.max(1)));
        for db_item in to_download {
            let graph = Arc::clone(&self.graph);
            let db = Arc::clone(&self.db);
            let tx = self.event_tx.clone();
            let cache_dir = cache_dir.clone();
            let item_local_path = db_item.local_path.clone();
            let id = db_item.id.clone();
            let etag = db_item.etag.clone();
            let remaining = Arc::clone(&remaining);
            let sem = Arc::clone(&sem);
            let lock = self.item_lock(&id);

            tokio::spawn(async move {
                let _permit = sem.acquire().await;
                let _guard = lock.lock().await;
                let cache_path = cache_dir.join(&id);
                if !cache_path.exists() {
                    // Pinning a folder can mean many large files; each one
                    // resumes rather than restarting if the run is interrupted.
                    match graph
                        .download_file_resumable(&id, &cache_path, etag.as_deref())
                        .await
                    {
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
                            if let Err(e2) = db
                                .set_sync_state(&id, &SyncState::Error(e.to_string()))
                                .await
                            {
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

        let cache_dir = self
            .cache_dir
            .as_ref()
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
            if let Err(e) = self
                .db
                .set_sync_state(&db_item.id, &SyncState::CloudOnly)
                .await
            {
                warn!(
                    "Unpin: failed to set CloudOnly state for {}: {e}",
                    db_item.id
                );
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

    // ── Cleanup ────────────────────────────────────────────────────────────────
}
