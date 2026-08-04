//! Cached drive quota.
//!
//! `GetQuota` used to call Microsoft Graph on every invocation. The status
//! window polls twice a second-and-a-half, so an open window meant a live HTTPS
//! round trip to Graph roughly every two seconds — around 1,800 requests an
//! hour, against an API that throttles, to re-read a number that changes maybe
//! once a day. The window's D-Bus client is synchronous, so it also stalled the
//! UI thread for the length of each round trip.
//!
//! The value is now cached. Callers get the last known figure immediately and a
//! refresh happens in the background when it is stale; only the very first call
//! ever waits on the network.

use graph_client::GraphClient;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// How long a reading is served before a refresh is started. Quota moves as
/// files are added and removed, so this is short enough to feel live without
/// being anywhere near per-poll.
const TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct Reading {
    used: u64,
    total: u64,
    fetched: Instant,
}

#[derive(Clone)]
pub struct QuotaCache {
    reading: Arc<Mutex<Option<Reading>>>,
    /// Guards against a burst of stale reads each spawning their own refresh.
    refreshing: Arc<AtomicBool>,
    graph: Arc<GraphClient>,
}

impl QuotaCache {
    pub fn new(graph: Arc<GraphClient>) -> Self {
        Self {
            reading: Arc::new(Mutex::new(None)),
            refreshing: Arc::new(AtomicBool::new(false)),
            graph,
        }
    }

    /// Used and total bytes. Blocks on the network only when nothing has been
    /// read yet; otherwise returns the cached figure and refreshes behind it.
    pub async fn get(&self) -> (u64, u64) {
        let cached = *self.reading.lock();

        match cached {
            Some(reading) if reading.fetched.elapsed() < TTL => (reading.used, reading.total),
            Some(reading) => {
                self.spawn_refresh();
                (reading.used, reading.total)
            }
            None => self.fetch().await.unwrap_or((0, 0)),
        }
    }

    /// Refresh in the background so the caller is not made to wait.
    fn spawn_refresh(&self) {
        // compare_exchange, not a load-then-store: two stale reads arriving
        // together would otherwise both start a fetch.
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.fetch().await;
            this.refreshing.store(false, Ordering::Release);
        });
    }

    async fn fetch(&self) -> Option<(u64, u64)> {
        match self.graph.get_drive().await {
            Ok(drive) => {
                let q = drive.quota.unwrap_or_default();
                *self.reading.lock() = Some(Reading {
                    used: q.used,
                    total: q.total,
                    fetched: Instant::now(),
                });
                debug!("Quota refreshed: {} / {}", q.used, q.total);
                Some((q.used, q.total))
            }
            Err(e) => {
                // Keep serving the previous reading rather than reporting zero:
                // a transient Graph failure should not make the storage meter
                // claim the drive is empty.
                warn!("Could not refresh quota: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the cache is that a status window polling every couple of
    /// seconds does not turn into a Graph request every couple of seconds.
    #[test]
    fn ttl_is_far_longer_than_the_ui_poll_interval() {
        // The flyout refreshes every 2s; anything close to that would defeat
        // the cache entirely.
        assert!(
            TTL >= Duration::from_secs(30),
            "a {TTL:?} TTL still means hundreds of Graph calls an hour"
        );
    }

    #[test]
    fn only_one_background_refresh_runs_at_a_time() {
        // Two stale reads arriving together must not both start a fetch.
        let flag = AtomicBool::new(false);
        let first = flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
        let second = flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
        assert!(first.is_ok(), "the first caller should win");
        assert!(second.is_err(), "the second caller must back off");
    }
}
