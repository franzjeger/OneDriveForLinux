//! Keeping the on-demand file cache within its configured size.
//!
//! Files On-Demand downloads every file you open and, before this, never
//! removed any of them: the cache only grew. That quietly gives back the disk
//! space the mode exists to save, so the cache now has a ceiling and evicts the
//! least recently used files when it is exceeded.
//!
//! Eviction is only ever safe because the file can be fetched again — so the
//! two cases where it cannot be are excluded outright: files the user pinned,
//! and files whose upload has not completed, where the cache file is the only
//! copy of their edit.

use super::*;

/// Files younger than this are never evicted, whatever their size. A file
/// downloaded moments ago is almost certainly the one being worked on, and
/// evicting it would mean downloading it again immediately.
const MIN_AGE_SECS: u64 = 3600;

/// One cache file considered for eviction.
struct Candidate {
    path: PathBuf,
    item_id: String,
    size: u64,
    /// Last read or write, whichever is later.
    last_used: std::time::SystemTime,
}

impl SyncEngine {
    /// Evict least-recently-used cache files until the cache fits its limit.
    ///
    /// Returns the number of bytes freed.
    pub async fn enforce_cache_limit(&self) -> u64 {
        if self.config.max_cache_size_gb <= 0.0 {
            return 0; // Explicitly unlimited.
        }
        let limit = (self.config.max_cache_size_gb * 1024.0 * 1024.0 * 1024.0) as u64;
        self.evict_down_to(limit, MIN_AGE_SECS).await
    }

    /// Bytes currently held by the on-demand cache.
    pub async fn cache_usage(&self) -> u64 {
        let Some(cache_dir) = &self.cache_dir else {
            return 0;
        };
        let Ok(dir) = std::fs::read_dir(cache_dir) else {
            return 0;
        };
        dir.flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    /// Evict everything that can be evicted, right now.
    ///
    /// Asked for explicitly, so the age grace does not apply: someone who
    /// chooses "free up space" means now, not "in an hour". Pinned files and
    /// files with an unsent edit are still never touched.
    pub async fn free_up_space(&self) -> u64 {
        self.evict_down_to(0, 0).await
    }

    /// Shared eviction: bring the cache down to `limit` bytes, sparing files
    /// used within `min_age_secs`.
    async fn evict_down_to(&self, limit: u64, min_age_secs: u64) -> u64 {
        let Some(cache_dir) = &self.cache_dir else {
            return 0;
        };

        // Protected sets, loaded once: pinned files are the user saying "keep
        // this", and a queued upload's cache file holds an edit that exists
        // nowhere else yet.
        let pinned = self.db.pinned_ids().await.unwrap_or_default();
        let queued = self.db.queued_upload_ids().await.unwrap_or_default();

        let (mut candidates, mut total) = (Vec::new(), 0u64);
        let now = std::time::SystemTime::now();

        let Ok(dir) = std::fs::read_dir(cache_dir) else {
            return 0;
        };
        for entry in dir.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();

            let Some(item_id) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            if is_protected(&item_id, &pinned, &queued) {
                continue;
            }

            // atime under `relatime` is coarse but still orders files by use far
            // better than mtime alone, which never moves for a file only read.
            let last_used = meta
                .accessed()
                .ok()
                .into_iter()
                .chain(meta.modified().ok())
                .max()
                .unwrap_or(now);
            if min_age_secs > 0
                && now
                    .duration_since(last_used)
                    .map(|age| age.as_secs() < min_age_secs)
                    .unwrap_or(true)
            {
                continue;
            }

            candidates.push(Candidate {
                path,
                item_id,
                size: meta.len(),
                last_used,
            });
        }

        if total <= limit {
            return 0;
        }

        let mut freed = 0u64;
        for candidate in plan_evictions(candidates, total, limit) {
            if let Err(e) = std::fs::remove_file(&candidate.path) {
                warn!("Cache eviction: could not remove {:?}: {e}", candidate.path);
                continue;
            }
            freed += candidate.size;

            // The FUSE layer re-downloads whenever the cache file is missing, so
            // the item stays usable; the state change is what makes the file
            // show as cloud-only again in the UI and in Dolphin.
            if let Err(e) = self
                .db
                .set_sync_state(&candidate.item_id, &SyncState::CloudOnly)
                .await
            {
                warn!(
                    "Cache eviction: could not mark {} cloud-only: {e}",
                    candidate.item_id
                );
            }
        }

        if freed > 0 {
            let mb = freed as f64 / (1024.0 * 1024.0);
            info!("Evicted {mb:.1} MB of least recently used cache files");
        } else {
            // Everything above the limit was pinned, queued, or too recent.
            // Say so: silently staying over the limit looks like the setting
            // being ignored.
            let gb = total as f64 / (1024.0 * 1024.0 * 1024.0);
            warn!(
                "Cache is {gb:.1} GB, over the {} GB limit, but nothing could be evicted \
                 (pinned, waiting to upload, or in use)",
                self.config.max_cache_size_gb
            );
        }
        freed
    }
}

/// Whether a cache file must never be evicted.
///
/// Pinned means the user asked for it to stay. A queued upload's cache file is
/// the only copy of an edit that has not reached OneDrive — deleting it would
/// destroy their work. `.tmp` files belong to in-flight downloads.
fn is_protected(
    item_id: &str,
    pinned: &std::collections::HashSet<String>,
    queued: &std::collections::HashSet<String>,
) -> bool {
    item_id.ends_with(".tmp") || pinned.contains(item_id) || queued.contains(item_id)
}

/// Choose which candidates to evict: least recently used first, stopping as
/// soon as the cache would fit within `limit`.
fn plan_evictions(mut candidates: Vec<Candidate>, total: u64, limit: u64) -> Vec<Candidate> {
    candidates.sort_by_key(|c| c.last_used);
    let mut freed = 0u64;
    let mut chosen = Vec::new();
    for candidate in candidates {
        if total.saturating_sub(freed) <= limit {
            break;
        }
        freed += candidate.size;
        chosen.push(candidate);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::{Duration, SystemTime};

    fn candidate(id: &str, size: u64, age_secs: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(format!("/cache/{id}")),
            item_id: id.into(),
            size,
            last_used: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
        }
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let candidates = vec![
            candidate("fresh", 100, 10),
            candidate("ancient", 100, 10_000),
            candidate("middling", 100, 1_000),
        ];
        // 300 bytes cached, 150 allowed: two files have to go.
        let ids: Vec<_> = plan_evictions(candidates, 300, 150)
            .into_iter()
            .map(|c| c.item_id)
            .collect();
        assert_eq!(ids, vec!["ancient", "middling"]);
    }

    #[test]
    fn stops_as_soon_as_the_cache_fits() {
        let candidates = vec![candidate("a", 100, 100), candidate("b", 100, 50)];
        // Already under the limit — nothing should be touched.
        assert!(plan_evictions(candidates, 150, 200).is_empty());
    }

    #[test]
    fn evicts_no_more_than_necessary() {
        let candidates = vec![
            candidate("a", 100, 300),
            candidate("b", 100, 200),
            candidate("c", 100, 100),
        ];
        let chosen = plan_evictions(candidates, 300, 250);
        assert_eq!(chosen.len(), 1, "freeing 50 bytes should cost one file");
        assert_eq!(chosen[0].item_id, "a");
    }

    #[test]
    fn an_explicit_free_up_takes_everything_evictable() {
        // free_up_space() passes limit 0, so nothing survives on size grounds.
        // What it must still respect is the protection list, which is applied
        // before candidates are ever built.
        let candidates = vec![candidate("a", 100, 10), candidate("b", 100, 10_000)];
        assert_eq!(plan_evictions(candidates, 200, 0).len(), 2);
    }

    #[test]
    fn pinned_and_queued_files_are_never_candidates() {
        let pinned: HashSet<String> = ["kept".to_string()].into_iter().collect();
        let queued: HashSet<String> = ["unsent".to_string()].into_iter().collect();

        assert!(
            is_protected("kept", &pinned, &queued),
            "pinned must survive"
        );
        assert!(
            is_protected("unsent", &pinned, &queued),
            "an unsent edit exists nowhere else — evicting it destroys the user's work"
        );
        assert!(is_protected("half-downloaded.tmp", &pinned, &queued));
        assert!(!is_protected("ordinary", &pinned, &queued));
    }
}
