//! Edits waiting out a quiet period before they are uploaded.
//!
//! Uploading the instant a file is closed is what made the mount fragile. An
//! atomic save — write a temp file, rename it over the target — closes the temp
//! file first, so the upload started under a name the file was about to stop
//! having. A file written and deleted seconds later raced its own upload. Rapid
//! saves of one file each started their own upload, and they collided with each
//! other on the way out.
//!
//! Waiting for a file to go quiet removes all of that at the source: by the
//! time the upload starts, the temp file has become the real one, the deleted
//! file is gone, and the ten saves have become one.
//!
//! What it costs is exactly as long as the wait: an edit lives only on this
//! machine until then. That is why [`PendingUploads::flush_now`] exists and why
//! the daemon calls it before shutting down — a restart must not take the wait
//! with it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

#[derive(Default)]
struct State {
    /// Item ID → when its upload becomes due. Rewriting a file pushes this out.
    due: HashMap<String, Instant>,
    /// Item IDs whose upload has started, kept until it finishes so a flush can
    /// wait for the bytes to actually land rather than merely to be sent.
    in_flight: HashSet<String>,
}

#[derive(Default)]
pub struct PendingUploads {
    state: Mutex<State>,
    /// Woken whenever a deadline moves, an entry is cancelled, or one finishes.
    changed: Notify,
}

impl PendingUploads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `item_id` has an edit to upload once it has been quiet for
    /// `delay`.
    ///
    /// Returns whether the caller should spawn a waiter. A second edit of a
    /// file already waiting just pushes its deadline out — which is the whole
    /// point, and is why an editor that saves continuously produces one upload
    /// rather than one per save.
    pub async fn schedule(&self, item_id: &str, delay: Duration) -> bool {
        let mut state = self.state.lock().await;
        let due_at = Instant::now() + delay;
        let is_new = !state.due.contains_key(item_id);
        state.due.insert(item_id.to_string(), due_at);
        drop(state);
        self.changed.notify_waiters();
        is_new
    }

    /// Block until this item's edit is due to upload.
    ///
    /// Returns false if it was cancelled while waiting — the file was deleted,
    /// and uploading it now would put it back.
    pub async fn wait_until_due(&self, item_id: &str) -> bool {
        loop {
            // Registered before the deadline is read, so a deadline moved
            // between the two is still seen. Missing that wakeup would leave
            // the waiter asleep on a deadline nobody is going to honour.
            let changed = self.changed.notified();

            let due_at = match self.state.lock().await.due.get(item_id) {
                Some(due_at) => *due_at,
                None => return false,
            };
            if due_at <= Instant::now() {
                return true;
            }

            tokio::select! {
                _ = tokio::time::sleep_until(due_at) => {}
                _ = changed => {}
            }
        }
    }

    /// Move an item from waiting to uploading. The returned guard keeps it
    /// counted until dropped.
    pub async fn begin_upload(self: &std::sync::Arc<Self>, item_id: &str) -> UploadGuard {
        let mut state = self.state.lock().await;
        state.due.remove(item_id);
        state.in_flight.insert(item_id.to_string());
        drop(state);
        self.changed.notify_waiters();
        UploadGuard {
            pending: std::sync::Arc::clone(self),
            item_id: item_id.to_string(),
        }
    }

    /// Drop a waiting edit — the file it belongs to no longer exists.
    pub async fn cancel(&self, item_id: &str) {
        let removed = self.state.lock().await.due.remove(item_id).is_some();
        if removed {
            self.changed.notify_waiters();
        }
    }

    /// Bring every waiting edit due immediately.
    ///
    /// The wait is a convenience, never a promise to hold data back. Anything
    /// that ends the session — shutting down, unmounting, the user asking —
    /// calls this first, so the longest an edit can be stranded on this machine
    /// is the debounce, not the time until the next launch.
    pub async fn flush_now(&self) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        for due_at in state.due.values_mut() {
            *due_at = now;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Wait until nothing is queued or uploading, giving up after `timeout`.
    /// Returns whether it drained.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            if self.count().await == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = changed => {}
            }
        }
    }

    /// Edits waiting or in flight — what the UI means by "not yet uploaded".
    pub async fn count(&self) -> usize {
        let state = self.state.lock().await;
        state.due.len() + state.in_flight.len()
    }
}

/// Keeps an upload counted as in flight until it finishes, however it finishes.
pub struct UploadGuard {
    pending: std::sync::Arc<PendingUploads>,
    item_id: String,
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        let pending = std::sync::Arc::clone(&self.pending);
        let item_id = std::mem::take(&mut self.item_id);
        // Drop is not async; the removal is trivial and must not be skipped, so
        // it goes on the runtime rather than blocking here.
        tokio::spawn(async move {
            pending.state.lock().await.in_flight.remove(&item_id);
            pending.changed.notify_waiters();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn an_edit_waits_for_its_delay() {
        let pending = Arc::new(PendingUploads::new());
        assert!(pending.schedule("a", Duration::from_secs(900)).await);

        let p = Arc::clone(&pending);
        let waiter = tokio::spawn(async move { p.wait_until_due("a").await });

        tokio::time::advance(Duration::from_secs(899)).await;
        assert!(!waiter.is_finished(), "uploaded before it went quiet");

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(waiter.await.unwrap(), "the edit should have come due");
    }

    #[tokio::test(start_paused = true)]
    async fn rewriting_pushes_the_deadline_out() {
        let pending = Arc::new(PendingUploads::new());
        assert!(pending.schedule("a", Duration::from_secs(900)).await);
        let p = Arc::clone(&pending);
        let waiter = tokio::spawn(async move { p.wait_until_due("a").await });

        tokio::time::advance(Duration::from_secs(800)).await;
        // A second save of the same file — one upload, not two.
        assert!(
            !pending.schedule("a", Duration::from_secs(900)).await,
            "a file already waiting must not get a second waiter"
        );

        tokio::time::advance(Duration::from_secs(200)).await;
        assert!(
            !waiter.is_finished(),
            "the deadline did not move — the upload would carry a half-written file"
        );

        tokio::time::advance(Duration::from_secs(800)).await;
        assert!(waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn deleting_the_file_cancels_the_upload() {
        let pending = Arc::new(PendingUploads::new());
        pending.schedule("a", Duration::from_secs(900)).await;
        let p = Arc::clone(&pending);
        let waiter = tokio::spawn(async move { p.wait_until_due("a").await });

        tokio::time::advance(Duration::from_secs(10)).await;
        pending.cancel("a").await;

        assert!(
            !waiter.await.unwrap(),
            "a deleted file must not upload — that is what put it back before"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flush_brings_everything_due_at_once() {
        let pending = Arc::new(PendingUploads::new());
        for id in ["a", "b", "c"] {
            pending.schedule(id, Duration::from_secs(900)).await;
        }
        let waiters: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|id| {
                let p = Arc::clone(&pending);
                tokio::spawn(async move { p.wait_until_due(id).await })
            })
            .collect();

        tokio::time::advance(Duration::from_secs(1)).await;
        pending.flush_now().await;

        for waiter in waiters {
            assert!(
                waiter.await.unwrap(),
                "a flush must release every waiting edit — this is what stops a \
                 restart taking the whole debounce window's work with it"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn count_covers_waiting_and_uploading() {
        let pending = Arc::new(PendingUploads::new());
        pending.schedule("a", Duration::from_secs(900)).await;
        assert_eq!(pending.count().await, 1);

        let guard = pending.begin_upload("a").await;
        assert_eq!(
            pending.count().await,
            1,
            "an upload in flight is still not on OneDrive"
        );

        drop(guard);
        tokio::task::yield_now().await;
        assert_eq!(pending.count().await, 0);
    }
}
