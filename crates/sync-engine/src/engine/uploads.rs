//! Retry loop for uploads that did not succeed the first time.
//!
//! Without this, a failed upload was logged and forgotten: the user's edit
//! stayed in the local cache while the database still claimed the file was
//! synced, so the app quietly disagreed with the server about the user's data.

use super::*;
use crate::db::PendingUpload;

/// How often the queue is inspected. Individual entries carry their own
/// `next_attempt`, so this only bounds how promptly a due retry starts.
const DRAIN_INTERVAL_SECS: u64 = 15;

/// Most entries attempted per pass, so one large backlog cannot starve
/// everything else the engine does.
const BATCH: usize = 8;

/// After this many failures an upload stops being retried automatically and is
/// reported to the user instead. Something is wrong that waiting will not fix.
pub const MAX_ATTEMPTS: u32 = 8;

/// Backoff before attempt number `attempts`: 30s, 1m, 2m, … capped at 30m.
pub fn retry_delay(attempts: u32) -> chrono::Duration {
    const BASE_SECS: i64 = 30;
    const MAX_SECS: i64 = 30 * 60;
    let secs = BASE_SECS
        .saturating_mul(1i64 << attempts.min(10))
        .min(MAX_SECS);
    chrono::Duration::seconds(secs)
}

impl SyncEngine {
    /// Queue an upload for retry, recording why the previous attempt failed.
    pub async fn queue_upload_retry(
        &self,
        item_id: &str,
        parent_id: &str,
        name: &str,
        source_path: &Path,
        attempts: u32,
        error: &str,
    ) {
        let entry = PendingUpload {
            item_id: item_id.to_string(),
            parent_id: parent_id.to_string(),
            name: name.to_string(),
            source_path: source_path.to_path_buf(),
            attempts,
            next_attempt: Utc::now() + retry_delay(attempts),
            last_error: Some(error.to_string()),
        };
        if let Err(e) = self.db.enqueue_upload(&entry).await {
            error!("Could not queue {name} for retry: {e} — the edit may be lost");
            return;
        }
        // Surface it: an item waiting to upload is not "Synced".
        if let Err(e) = self.db.set_sync_state(item_id, &SyncState::LocalOnly).await {
            warn!("Failed to mark {item_id} as pending upload: {e}");
        }
        warn!(
            "Upload of {name} failed (attempt {attempts}), retrying in {}s: {error}",
            retry_delay(attempts).num_seconds()
        );
    }

    /// Uploads still waiting to succeed.
    pub async fn pending_uploads(&self) -> usize {
        self.db.pending_upload_count().await.unwrap_or(0)
    }

    /// Background loop draining the upload queue.
    pub(super) async fn upload_retry_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(DRAIN_INTERVAL_SECS)).await;
            if self.is_paused().await {
                continue;
            }
            self.drain_upload_queue().await;
        }
    }

    /// Attempt every upload whose retry time has arrived.
    pub async fn drain_upload_queue(&self) {
        let due = match self.db.due_uploads(BATCH).await {
            Ok(due) => due,
            Err(e) => {
                error!("Could not read the upload queue: {e}");
                return;
            }
        };
        if due.is_empty() {
            return;
        }
        info!("Retrying {} queued upload(s)", due.len());

        for entry in due {
            // The cache file is the content. If it is gone there is nothing
            // left to upload, and keeping the entry would retry forever.
            if !entry.source_path.exists() {
                warn!(
                    "Dropping queued upload for {}: {:?} no longer exists",
                    entry.name, entry.source_path
                );
                let _ = self.db.dequeue_upload(&entry.item_id).await;
                continue;
            }

            let _guard = self.item_lock(&entry.item_id).lock_owned().await;
            match self
                .graph
                .upload_file(&entry.parent_id, &entry.name, &entry.source_path)
                .await
            {
                Ok(updated) => {
                    if let Err(e) = self.db.dequeue_upload(&entry.item_id).await {
                        warn!("Upload of {} succeeded but dequeue failed: {e}", entry.name);
                    }
                    self.mark_uploaded(&entry, updated).await;
                    info!("Queued upload of {} succeeded", entry.name);
                }
                Err(e) => {
                    let attempts = entry.attempts + 1;
                    if attempts >= MAX_ATTEMPTS {
                        error!(
                            "Giving up on uploading {} after {attempts} attempts: {e}",
                            entry.name
                        );
                        let _ = self.db.dequeue_upload(&entry.item_id).await;
                        let _ = self
                            .db
                            .set_sync_state(&entry.item_id, &SyncState::Error(e.to_string()))
                            .await;
                        let _ = self.event_tx.send(SyncEvent::UploadFailed {
                            name: entry.name.clone(),
                            error: e.to_string(),
                        });
                    } else {
                        self.queue_upload_retry(
                            &entry.item_id,
                            &entry.parent_id,
                            &entry.name,
                            &entry.source_path,
                            attempts,
                            &e.to_string(),
                        )
                        .await;
                    }
                }
            }
        }
    }

    /// Record a successful upload against the item's current database row.
    async fn mark_uploaded(&self, entry: &PendingUpload, updated: DriveItem) {
        // Re-read: a rename may have changed name/local_path while we uploaded.
        let Ok(Some(mut item)) = self.db.get_item_by_id(&entry.item_id).await else {
            return;
        };
        item.size = updated.size.unwrap_or_else(|| {
            std::fs::metadata(&entry.source_path)
                .map(|m| m.len())
                .unwrap_or(0)
        });
        item.etag = updated.e_tag;
        item.ctag = updated.c_tag;
        item.modified_at = updated.last_modified_date_time;
        item.is_placeholder = false;
        item.sync_state = SyncState::Synced;
        let path = item.local_path.clone();
        if let Err(e) = self.db.upsert_item(&item).await {
            error!("Failed to record uploaded item {}: {e}", entry.name);
            return;
        }
        let _ = self.event_tx.send(SyncEvent::ItemStateChanged {
            path,
            state: SyncState::Synced,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_saturates() {
        assert_eq!(retry_delay(0).num_seconds(), 30);
        assert_eq!(retry_delay(1).num_seconds(), 60);
        assert_eq!(retry_delay(2).num_seconds(), 120);
        // Capped rather than growing without bound.
        assert_eq!(retry_delay(20).num_seconds(), 30 * 60);
    }

    #[test]
    fn backoff_never_overflows() {
        // A corrupt attempts value must not panic the retry loop.
        assert_eq!(retry_delay(u32::MAX).num_seconds(), 30 * 60);
    }

    #[test]
    fn giving_up_takes_long_enough_to_outlast_an_outage() {
        let total: i64 = (0..MAX_ATTEMPTS)
            .map(|n| retry_delay(n).num_seconds())
            .sum();
        assert!(
            total >= 30 * 60,
            "retries exhaust after {total}s — too quick to survive a short outage"
        );
    }
}
