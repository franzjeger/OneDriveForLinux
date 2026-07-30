use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tracing::warn;

/// Patterns for temp/lock files that should never be synced.
const IGNORED_SUFFIXES: &[&str] = &[".tmp", ".part", ".crdownload", ".swp", ".swo", ".kate-swp"];
const IGNORED_PREFIXES: &[&str] = &["~$", ".~lock.", ".nfs"];

/// Debounce window — events within this window for the same path are coalesced.
const DEBOUNCE_MS: u64 = 500;

pub struct LocalWatcher {
    _watcher: RecommendedWatcher,
    pub events: mpsc::Receiver<Event>,
}

impl LocalWatcher {
    pub fn new(root: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Event>(512);

        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<Event>| match result {
                Ok(event) => {
                    if should_ignore_event(&event) {
                        return;
                    }
                    if let Err(e) = tx.blocking_send(event) {
                        warn!("Watcher channel send error: {e}");
                    }
                }
                Err(e) => warn!("Watcher error: {e}"),
            })?;

        watcher.watch(root, RecursiveMode::Recursive)?;
        tracing::info!("Local watcher started: watching {:?}", root);

        Ok(Self {
            _watcher: watcher,
            events: rx,
        })
    }
}

fn should_ignore_event(event: &Event) -> bool {
    if event.paths.is_empty() {
        return false;
    }
    // On Linux, notify coalesces IN_MOVED_FROM + IN_MOVED_TO into a single
    // Modify(Name(Both)) event with event.paths = [source, destination].
    // We must only ignore the event if ALL paths are ignorable — otherwise a
    // rename from "file.tmp" → "document.docx" would be silently dropped
    // because the source path matches the .tmp suffix filter.
    event.paths.iter().all(|path| {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s))
            || IGNORED_PREFIXES.iter().any(|p| name.starts_with(p))
    })
}

/// Debouncer that collapses rapid-fire events on the same path into one.
pub struct EventDebouncer {
    pending: HashMap<PathBuf, (Event, Instant)>,
    debounce: Duration,
}

impl EventDebouncer {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            debounce: Duration::from_millis(DEBOUNCE_MS),
        }
    }

    /// Feed an event in. Returns events that have passed the debounce window.
    ///
    /// For a burst of events on the same path (Create → Modify → Access(Close(Write))),
    /// we keep the FIRST event kind (usually Create or Modify, which are actionable)
    /// but update the timestamp on every subsequent event to extend the debounce window
    /// until the burst settles. This prevents the final Access(Close(Write)) event from
    /// silently replacing an actionable Create/Modify event.
    pub fn feed(&mut self, event: Event) -> Vec<Event> {
        let now = Instant::now();
        for path in &event.paths {
            let entry = self
                .pending
                .entry(path.clone())
                .or_insert_with(|| (event.clone(), now));
            // Always extend the debounce window on each new event for this path.
            entry.1 = now;
        }
        self.drain_ready(now)
    }

    /// Check for any events that have waited long enough.
    pub fn drain_ready(&mut self, now: Instant) -> Vec<Event> {
        let debounce = self.debounce;
        let mut ready = Vec::new();
        self.pending.retain(|_, (event, inserted_at)| {
            if now.duration_since(*inserted_at) >= debounce {
                ready.push(event.clone());
                false
            } else {
                true
            }
        });
        ready
    }

    /// Flush everything regardless of debounce (e.g. on shutdown).
    pub fn flush(&mut self) -> Vec<Event> {
        self.pending.drain().map(|(_, (ev, _))| ev).collect()
    }

    pub fn is_create_or_modify(kind: &EventKind) -> bool {
        // Match all Create and Modify variants. In notify 6 on Linux, atomic saves
        // (LibreOffice, vim, etc.) generate Modify(Name(To)) which is covered by
        // the Modify(_) arm.
        matches!(kind, EventKind::Create(_) | EventKind::Modify(_))
    }

    pub fn is_remove(kind: &EventKind) -> bool {
        matches!(kind, EventKind::Remove(_))
    }
}

impl Default for EventDebouncer {
    fn default() -> Self {
        Self::new()
    }
}
