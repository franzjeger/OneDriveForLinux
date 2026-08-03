use crate::state::SyncState;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

/// Represents a row in the `items` table.
#[derive(Debug, Clone)]
pub struct DbItem {
    pub id: String,
    pub local_path: PathBuf,
    pub name: String,
    pub parent_id: Option<String>,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub size: u64,
    pub modified_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub sha1_hash: Option<String>,
    pub quick_xor_hash: Option<String>,
    pub is_folder: bool,
    pub is_placeholder: bool,
    pub sync_state: SyncState,
    /// User-pinned: always kept on device, never evicted to cloud-only.
    /// Never overwritten by delta sync — only changed via pin/unpin commands.
    pub pinned: bool,
}

/// Thread-safe SQLite wrapper.
///
/// Uses `parking_lot::Mutex` instead of `std::sync::Mutex` to avoid mutex
/// poisoning — a panic in one thread won't cascade to all other DB callers.
/// All public methods are async and run the blocking SQLite work on Tokio's
/// blocking thread pool via `spawn_blocking`, preventing Tokio worker thread
/// starvation under database contention.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open database {path:?}"))?;
        let db = Database {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;

             CREATE TABLE IF NOT EXISTS items (
                 id              TEXT PRIMARY KEY,
                 local_path      TEXT NOT NULL UNIQUE,
                 name            TEXT NOT NULL,
                 parent_id       TEXT,
                 etag            TEXT,
                 ctag            TEXT,
                 size            INTEGER NOT NULL DEFAULT 0,
                 modified_at     TEXT,
                 created_at      TEXT,
                 sha1_hash       TEXT,
                 quick_xor_hash  TEXT,
                 is_folder       INTEGER NOT NULL DEFAULT 0,
                 is_placeholder  INTEGER NOT NULL DEFAULT 0,
                 sync_state      TEXT NOT NULL DEFAULT 'synced',
                 pinned          INTEGER NOT NULL DEFAULT 0
             );

             CREATE INDEX IF NOT EXISTS idx_items_local_path   ON items(local_path);
             CREATE INDEX IF NOT EXISTS idx_items_parent_id    ON items(parent_id);
             CREATE INDEX IF NOT EXISTS idx_items_parent_name  ON items(parent_id, name);

             CREATE TABLE IF NOT EXISTS delta_links (
                 folder_id   TEXT PRIMARY KEY,
                 delta_link  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS sync_excluded (
                 pattern TEXT PRIMARY KEY
             );

             CREATE TABLE IF NOT EXISTS local_symlinks (
                 parent_path TEXT NOT NULL,
                 name        TEXT NOT NULL,
                 target      TEXT NOT NULL,
                 PRIMARY KEY (parent_path, name)
             );",
        )
        .context("database migration")?;

        // Add `pinned` column to existing databases that pre-date this migration.
        // Ignore error — it just means the column already exists.
        let _ = conn.execute(
            "ALTER TABLE items ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );

        // Partial index on pinned — created after ALTER TABLE so the column is guaranteed to exist.
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_items_pinned ON items(pinned) WHERE pinned = 1",
            [],
        );

        Ok(())
    }

    /// Run a blocking database operation on Tokio's blocking thread pool.
    /// This prevents SQLite I/O from starving async tasks on worker threads.
    async fn with_conn<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            f(&conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("db task panicked: {e}"))?
    }

    // ── Items ──────────────────────────────────────────────────────────────────

    pub async fn upsert_item(&self, item: &DbItem) -> Result<()> {
        let item = item.clone();
        self.with_conn(move |conn| {
            // If another item already occupies this local_path (e.g. a stale entry
            // from a previous delta sync with a different item ID), remove it first.
            // This prevents UNIQUE constraint violations on local_path when FUSE
            // creates new items that shadow stale DB entries.
            conn.execute(
                "DELETE FROM items WHERE local_path = ?1 AND id != ?2",
                params![item.local_path.to_string_lossy().as_ref(), item.id],
            )?;
            conn.execute(
                // `pinned` is intentionally excluded from the ON CONFLICT UPDATE clause —
                // it is user-controlled and must never be overwritten by delta sync.
                "INSERT INTO items
                     (id, local_path, name, parent_id, etag, ctag, size,
                      modified_at, created_at, sha1_hash, quick_xor_hash,
                      is_folder, is_placeholder, sync_state, pinned)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                 ON CONFLICT(id) DO UPDATE SET
                     local_path     = excluded.local_path,
                     name           = excluded.name,
                     parent_id      = excluded.parent_id,
                     etag           = excluded.etag,
                     ctag           = excluded.ctag,
                     size           = excluded.size,
                     modified_at    = excluded.modified_at,
                     created_at     = excluded.created_at,
                     sha1_hash      = excluded.sha1_hash,
                     quick_xor_hash = excluded.quick_xor_hash,
                     is_folder      = excluded.is_folder,
                     is_placeholder = excluded.is_placeholder,
                     sync_state     = excluded.sync_state",
                params![
                    item.id,
                    item.local_path.to_string_lossy().as_ref(),
                    item.name,
                    item.parent_id,
                    item.etag,
                    item.ctag,
                    item.size as i64,
                    item.modified_at.map(|d| d.to_rfc3339()),
                    item.created_at.map(|d| d.to_rfc3339()),
                    item.sha1_hash,
                    item.quick_xor_hash,
                    item.is_folder as i32,
                    item.is_placeholder as i32,
                    item.sync_state.as_db_str(),
                    item.pinned as i32,
                ],
            )?;
            debug!("upserted item {}", item.id);
            Ok(())
        })
        .await
    }

    pub async fn get_item_by_path(&self, path: &Path) -> Result<Option<DbItem>> {
        let path_str = path.to_string_lossy().to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, local_path, name, parent_id, etag, ctag, size,
                        modified_at, created_at, sha1_hash, quick_xor_hash,
                        is_folder, is_placeholder, sync_state, pinned
                 FROM items WHERE local_path = ?1",
            )?;
            let mut rows = stmt.query(params![path_str])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row_to_item(row)?))
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn get_item_by_id(&self, id: &str) -> Result<Option<DbItem>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, local_path, name, parent_id, etag, ctag, size,
                        modified_at, created_at, sha1_hash, quick_xor_hash,
                        is_folder, is_placeholder, sync_state, pinned
                 FROM items WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row_to_item(row)?))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// Items directly under `sync_dir` (depth 1) — used to bootstrap the FUSE root drive ID.
    pub async fn get_root_drive_id(&self, sync_dir: &Path) -> Result<Option<String>> {
        let sync_dir = sync_dir.to_path_buf();
        self.with_conn(move |conn| {
            let glob1 = format!("{}/*", sync_dir.to_string_lossy());
            let glob2 = format!("{}/*/*", sync_dir.to_string_lossy());
            let mut stmt = conn.prepare_cached(
                "SELECT parent_id FROM items
                 WHERE local_path GLOB ?1 AND local_path NOT GLOB ?2
                 AND parent_id IS NOT NULL LIMIT 1",
            )?;
            let mut rows = stmt.query(params![glob1, glob2])?;
            if let Some(row) = rows.next()? {
                Ok(row.get(0)?)
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// Synchronous version for use at startup (before async runtime is fully running).
    pub fn get_root_drive_id_sync(&self, sync_dir: &Path) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let glob1 = format!("{}/*", sync_dir.to_string_lossy());
        let glob2 = format!("{}/*/*", sync_dir.to_string_lossy());
        let mut stmt = conn.prepare_cached(
            "SELECT parent_id FROM items
             WHERE local_path GLOB ?1 AND local_path NOT GLOB ?2
             AND parent_id IS NOT NULL LIMIT 1",
        )?;
        let mut rows = stmt.query(params![glob1, glob2])?;
        if let Some(row) = rows.next()? {
            Ok(row.get(0)?)
        } else {
            Ok(None)
        }
    }

    /// All children of the given parent item ID, ordered by name.
    pub async fn get_children(&self, parent_id: &str) -> Result<Vec<DbItem>> {
        let parent_id = parent_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, local_path, name, parent_id, etag, ctag, size,
                        modified_at, created_at, sha1_hash, quick_xor_hash,
                        is_folder, is_placeholder, sync_state, pinned
                 FROM items WHERE parent_id = ?1 ORDER BY name",
            )?;
            let items: Result<Vec<DbItem>, _> = stmt
                .query_map(params![parent_id], |row| {
                    row_to_item(row).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(e.to_string())),
                        )
                    })
                })?
                .map(|r| r.map_err(anyhow::Error::from))
                .collect();
            items
        })
        .await
    }

    /// Single child lookup by parent + name (fast point query via index).
    pub async fn get_child_by_name(&self, parent_id: &str, name: &str) -> Result<Option<DbItem>> {
        let parent_id = parent_id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, local_path, name, parent_id, etag, ctag, size,
                        modified_at, created_at, sha1_hash, quick_xor_hash,
                        is_folder, is_placeholder, sync_state, pinned
                 FROM items WHERE parent_id = ?1 AND name = ?2",
            )?;
            let mut rows = stmt.query(params![parent_id, name])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row_to_item(row)?))
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn all_items(&self) -> Result<Vec<DbItem>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, local_path, name, parent_id, etag, ctag, size,
                        modified_at, created_at, sha1_hash, quick_xor_hash,
                        is_folder, is_placeholder, sync_state, pinned
                 FROM items ORDER BY local_path",
            )?;
            let items: Result<Vec<DbItem>, _> = stmt
                .query_map([], |row| {
                    row_to_item(row).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(e.to_string())),
                        )
                    })
                })?
                .map(|r| r.map_err(anyhow::Error::from))
                .collect();
            items
        })
        .await
    }

    pub async fn delete_item(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    pub async fn set_sync_state(&self, id: &str, state: &SyncState) -> Result<()> {
        let id = id.to_string();
        let state_str = match state {
            SyncState::Error(msg) => format!("error:{msg}"),
            other => other.as_db_str().to_string(),
        };
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE items SET sync_state = ?1 WHERE id = ?2",
                params![state_str, id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn set_placeholder(&self, id: &str, is_placeholder: bool) -> Result<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE items SET is_placeholder = ?1 WHERE id = ?2",
                params![is_placeholder as i32, id],
            )?;
            Ok(())
        })
        .await
    }

    // ── Pin / unpin ────────────────────────────────────────────────────────────

    /// IDs of every pinned item. Loading these once lets a delta pass decide
    /// pinned-vs-placeholder without a per-item DB round trip.
    pub async fn pinned_ids(&self) -> Result<std::collections::HashSet<String>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare("SELECT id FROM items WHERE pinned = 1")?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(ids)
        })
        .await
    }

    /// Pin or unpin a single item by its OneDrive item ID.
    pub async fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE items SET pinned = ?1 WHERE id = ?2",
                params![pinned as i32, id],
            )?;
            Ok(())
        })
        .await
    }

    /// Pin or unpin an item and all items recursively under it (by local path prefix).
    pub async fn set_pinned_for_path(&self, path: &Path, pinned: bool) -> Result<usize> {
        let path_str = path.to_string_lossy().to_string();
        self.with_conn(move |conn| {
            // Match the exact path OR anything under it.
            let glob = format!("{path_str}/*");
            let n = conn.execute(
                "UPDATE items SET pinned = ?1 WHERE local_path = ?2 OR local_path GLOB ?3",
                params![pinned as i32, path_str, glob],
            )?;
            Ok(n)
        })
        .await
    }

    /// All non-folder items whose local_path is under `path` (recursive).
    pub async fn get_files_under(&self, path: &Path) -> Result<Vec<DbItem>> {
        let glob = format!("{}/*", path.to_string_lossy());
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, local_path, name, parent_id, etag, ctag, size,
                        modified_at, created_at, sha1_hash, quick_xor_hash,
                        is_folder, is_placeholder, sync_state, pinned
                 FROM items WHERE local_path GLOB ?1 AND is_folder = 0",
            )?;
            let items: Result<Vec<DbItem>, _> = stmt
                .query_map(params![glob], |row| {
                    row_to_item(row).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(e.to_string())),
                        )
                    })
                })?
                .map(|r| r.map_err(anyhow::Error::from))
                .collect();
            items
        })
        .await
    }

    /// Upsert many items in a single transaction — much faster than one-by-one
    /// for large batches (e.g. initial full-delta sync).
    pub async fn upsert_items_batch(&self, items: Vec<DbItem>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.with_conn(move |conn| {
            conn.execute_batch("BEGIN")?;
            let result: Result<()> = (|| {
                for item in &items {
                    conn.execute(
                        "INSERT INTO items
                             (id, local_path, name, parent_id, etag, ctag, size,
                              modified_at, created_at, sha1_hash, quick_xor_hash,
                              is_folder, is_placeholder, sync_state, pinned)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                         ON CONFLICT(id) DO UPDATE SET
                             local_path     = excluded.local_path,
                             name           = excluded.name,
                             parent_id      = excluded.parent_id,
                             etag           = excluded.etag,
                             ctag           = excluded.ctag,
                             size           = excluded.size,
                             modified_at    = excluded.modified_at,
                             created_at     = excluded.created_at,
                             sha1_hash      = excluded.sha1_hash,
                             quick_xor_hash = excluded.quick_xor_hash,
                             is_folder      = excluded.is_folder,
                             is_placeholder = excluded.is_placeholder,
                             sync_state     = excluded.sync_state",
                        params![
                            item.id,
                            item.local_path.to_string_lossy().as_ref(),
                            item.name,
                            item.parent_id,
                            item.etag,
                            item.ctag,
                            item.size as i64,
                            item.modified_at.map(|d| d.to_rfc3339()),
                            item.created_at.map(|d| d.to_rfc3339()),
                            item.sha1_hash,
                            item.quick_xor_hash,
                            item.is_folder as i32,
                            item.is_placeholder as i32,
                            item.sync_state.as_db_str(),
                            item.pinned as i32,
                        ],
                    )?;
                }
                Ok(())
            })();
            if result.is_ok() {
                conn.execute_batch("COMMIT")?;
            } else {
                let _ = conn.execute_batch("ROLLBACK");
            }
            result
        })
        .await
    }

    /// All items (files and folders) under any of the given path prefixes.
    /// Used for post-full-sync reconciliation to detect remote deletions.
    pub async fn get_items_under_paths(&self, paths: &[std::path::PathBuf]) -> Result<Vec<DbItem>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        let paths = paths.to_vec();
        self.with_conn(move |conn| {
            let mut all = Vec::new();
            for path in &paths {
                let prefix = path.to_string_lossy().to_string();
                let glob = format!("{prefix}/*");
                let mut stmt = conn.prepare_cached(
                    "SELECT id, local_path, name, parent_id, etag, ctag, size,
                            modified_at, created_at, sha1_hash, quick_xor_hash,
                            is_folder, is_placeholder, sync_state, pinned
                     FROM items WHERE local_path = ?1 OR local_path GLOB ?2",
                )?;
                let items: Vec<DbItem> = stmt
                    .query_map(params![prefix, glob], |row| {
                        row_to_item(row).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::other(e.to_string())),
                            )
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                all.extend(items);
            }
            Ok(all)
        })
        .await
    }

    /// Compute aggregate sync state for a folder from its descendants.
    pub async fn get_folder_aggregate_state(&self, folder_local_path: &Path) -> Result<SyncState> {
        let glob = format!("{}/*", folder_local_path.to_string_lossy());
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT
                     (SELECT 1 FROM items
                      WHERE local_path GLOB ?1 AND pinned = 1
                      LIMIT 1) AS has_pinned,
                     (SELECT 1 FROM items
                      WHERE local_path GLOB ?1 AND is_folder = 0 AND sync_state != 'cloud_only'
                      LIMIT 1) AS has_synced,
                     (SELECT 1 FROM items
                      WHERE local_path GLOB ?1 AND is_folder = 0 AND sync_state = 'cloud_only'
                      LIMIT 1) AS has_cloud",
            )?;
            let mut rows = stmt.query(params![glob])?;
            if let Some(row) = rows.next()? {
                let has_pinned: Option<i64> = row.get(0)?;
                let has_synced: Option<i64> = row.get(1)?;
                let has_cloud: Option<i64> = row.get(2)?;
                if has_pinned.is_some() {
                    return Ok(SyncState::Pinned);
                }
                if has_synced.is_some() && has_cloud.is_some() {
                    return Ok(SyncState::Partial);
                }
                if has_synced.is_some() {
                    return Ok(SyncState::Synced);
                }
            }
            Ok(SyncState::CloudOnly)
        })
        .await
    }

    // ── Local symlinks (FUSE-only, never synced to OneDrive) ─────────────────

    /// Create a local-only symlink entry.
    pub async fn create_symlink(&self, parent_path: &Path, name: &str, target: &str) -> Result<()> {
        let parent_str = parent_path.to_string_lossy().to_string();
        let name = name.to_string();
        let target = target.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO local_symlinks (parent_path, name, target) VALUES (?1, ?2, ?3)",
                params![parent_str, name, target],
            )?;
            Ok(())
        })
        .await
    }

    /// Read a symlink target.
    pub async fn get_symlink(&self, parent_path: &Path, name: &str) -> Result<Option<String>> {
        let parent_str = parent_path.to_string_lossy().to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT target FROM local_symlinks WHERE parent_path = ?1 AND name = ?2",
            )?;
            let mut rows = stmt.query(params![parent_str, name])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row.get(0)?))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// List all symlinks under a parent path.
    pub async fn get_symlinks_in(&self, parent_path: &Path) -> Result<Vec<(String, String)>> {
        let parent_str = parent_path.to_string_lossy().to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT name, target FROM local_symlinks WHERE parent_path = ?1")?;
            let rows = stmt.query_map(params![parent_str], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .await
    }

    /// Delete a symlink.
    pub async fn delete_symlink(&self, parent_path: &Path, name: &str) -> Result<()> {
        let parent_str = parent_path.to_string_lossy().to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM local_symlinks WHERE parent_path = ?1 AND name = ?2",
                params![parent_str, name],
            )?;
            Ok(())
        })
        .await
    }

    // ── Delta links ────────────────────────────────────────────────────────────

    pub async fn set_delta_link(&self, folder_id: &str, delta_link: &str) -> Result<()> {
        let folder_id = folder_id.to_string();
        let delta_link = delta_link.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO delta_links (folder_id, delta_link, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(folder_id) DO UPDATE SET
                     delta_link = excluded.delta_link,
                     updated_at = excluded.updated_at",
                params![folder_id, delta_link, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_delta_link(&self, folder_id: &str) -> Result<Option<String>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare_cached("SELECT delta_link FROM delta_links WHERE folder_id = ?1")?;
            let mut rows = stmt.query(params![folder_id])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row.get(0)?))
            } else {
                Ok(None)
            }
        })
        .await
    }
}

fn row_to_item(row: &rusqlite::Row) -> Result<DbItem> {
    let path_str: String = row.get(1)?;
    let sync_state_str: String = row.get(13)?;
    let sync_state = if let Some(msg) = sync_state_str.strip_prefix("error:") {
        SyncState::Error(msg.to_string())
    } else {
        SyncState::from_db_str(&sync_state_str)
    };

    let modified_at: Option<String> = row.get(7)?;
    let created_at: Option<String> = row.get(8)?;

    Ok(DbItem {
        id: row.get(0)?,
        local_path: PathBuf::from(path_str),
        name: row.get(2)?,
        parent_id: row.get(3)?,
        etag: row.get(4)?,
        ctag: row.get(5)?,
        size: row.get::<_, i64>(6)? as u64,
        modified_at: modified_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc)),
        created_at: created_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc)),
        sha1_hash: row.get(9)?,
        quick_xor_hash: row.get(10)?,
        is_folder: row.get::<_, i32>(11)? != 0,
        is_placeholder: row.get::<_, i32>(12)? != 0,
        sync_state,
        pinned: row.get::<_, i32>(14).unwrap_or(0) != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_item(id: &str, path: &str) -> DbItem {
        DbItem {
            id: id.to_string(),
            local_path: PathBuf::from(path),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            parent_id: None,
            etag: Some("etag1".into()),
            ctag: None,
            size: 42,
            modified_at: None,
            created_at: None,
            sha1_hash: None,
            quick_xor_hash: None,
            is_folder: false,
            is_placeholder: false,
            sync_state: SyncState::Synced,
            pinned: false,
        }
    }

    fn open_temp_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn upsert_and_get_roundtrip() {
        let (_dir, db) = open_temp_db();
        let item = test_item("id1", "/sync/doc.txt");
        db.upsert_item(&item).await.unwrap();

        let by_id = db.get_item_by_id("id1").await.unwrap().unwrap();
        assert_eq!(by_id.name, "doc.txt");
        assert_eq!(by_id.size, 42);
        assert_eq!(by_id.sync_state, SyncState::Synced);

        let by_path = db
            .get_item_by_path(Path::new("/sync/doc.txt"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_path.id, "id1");
    }

    #[tokio::test]
    async fn pinned_ids_returns_only_pinned_items() {
        let (_dir, db) = open_temp_db();
        db.upsert_item(&test_item("plain", "/sync/a.txt"))
            .await
            .unwrap();
        db.upsert_item(&test_item("kept", "/sync/b.txt"))
            .await
            .unwrap();
        db.set_pinned("kept", true).await.unwrap();

        let pinned = db.pinned_ids().await.unwrap();
        assert_eq!(pinned.len(), 1);
        assert!(pinned.contains("kept"));
    }

    #[tokio::test]
    async fn upsert_replaces_stale_entry_at_same_path() {
        let (_dir, db) = open_temp_db();
        db.upsert_item(&test_item("old", "/sync/a.txt"))
            .await
            .unwrap();
        db.upsert_item(&test_item("new", "/sync/a.txt"))
            .await
            .unwrap();

        assert!(db.get_item_by_id("old").await.unwrap().is_none());
        assert_eq!(
            db.get_item_by_path(Path::new("/sync/a.txt"))
                .await
                .unwrap()
                .unwrap()
                .id,
            "new"
        );
    }

    #[tokio::test]
    async fn delta_sync_never_overwrites_pinned() {
        let (_dir, db) = open_temp_db();
        db.upsert_item(&test_item("id1", "/sync/a.txt"))
            .await
            .unwrap();
        db.set_pinned("id1", true).await.unwrap();

        // A later delta upsert (pinned defaults to false) must not clear the pin.
        db.upsert_item(&test_item("id1", "/sync/a.txt"))
            .await
            .unwrap();
        assert!(db.get_item_by_id("id1").await.unwrap().unwrap().pinned);
    }

    #[tokio::test]
    async fn set_sync_state_and_delete() {
        let (_dir, db) = open_temp_db();
        db.upsert_item(&test_item("id1", "/sync/a.txt"))
            .await
            .unwrap();

        db.set_sync_state("id1", &SyncState::CloudOnly)
            .await
            .unwrap();
        assert_eq!(
            db.get_item_by_id("id1").await.unwrap().unwrap().sync_state,
            SyncState::CloudOnly
        );

        db.delete_item("id1").await.unwrap();
        assert!(db.get_item_by_id("id1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delta_link_roundtrip() {
        let (_dir, db) = open_temp_db();
        assert!(db.get_delta_link("root").await.unwrap().is_none());
        db.set_delta_link("root", "https://example/delta?token=1")
            .await
            .unwrap();
        assert_eq!(
            db.get_delta_link("root").await.unwrap().unwrap(),
            "https://example/delta?token=1"
        );
    }

    #[tokio::test]
    async fn symlink_roundtrip() {
        let (_dir, db) = open_temp_db();
        let parent = Path::new("/sync");
        db.create_symlink(parent, "link", "/target").await.unwrap();
        assert_eq!(
            db.get_symlink(parent, "link").await.unwrap().unwrap(),
            "/target"
        );
        assert_eq!(db.get_symlinks_in(parent).await.unwrap().len(), 1);
        db.delete_symlink(parent, "link").await.unwrap();
        assert!(db.get_symlink(parent, "link").await.unwrap().is_none());
    }
}
