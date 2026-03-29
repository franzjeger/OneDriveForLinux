use fuse3::{
    raw::{
        reply::{
            DirectoryEntry, DirectoryEntryPlus, FileAttr, ReplyAttr, ReplyCreated, ReplyData,
            ReplyDirectory, ReplyDirectoryPlus, ReplyEntry, ReplyInit, ReplyOpen, ReplyStatFs,
            ReplyWrite, ReplyXAttr,
        },
        Filesystem, Request,
    },
    FileType, Result as FuseResult, SetAttr, Timestamp,
};
use futures::stream;
use graph_client::GraphClient;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    os::unix::fs::FileExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::SystemTime,
};
use sync_engine::{Database, DbItem, SyncState};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

const TTL_SEC: u64 = 1;

/// Inode counter — starts at 2 (1 is always root).
static INODE_COUNTER: AtomicU64 = AtomicU64::new(2);

fn next_inode() -> u64 {
    INODE_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn sys_time_to_ts(t: SystemTime) -> Timestamp {
    Timestamp::from(t)
}

fn epoch_ts() -> Timestamp {
    Timestamp::new(0, 0)
}

/// In-memory inode table entry.
#[derive(Clone)]
struct InodeEntry {
    inode: u64,
    item_id: String,
    parent_inode: u64,
    is_dir: bool,
}

/// In-memory symlink inode entry (local-only, not synced to OneDrive).
#[derive(Clone)]
struct SymlinkEntry {
    inode: u64,
    parent_inode: u64,
    name: String,
    target: String,
}

pub struct OneDriveFS {
    db: Arc<Database>,
    graph: Arc<GraphClient>,
    /// inode → InodeEntry
    inodes: RwLock<BTreeMap<u64, InodeEntry>>,
    /// item_id → inode
    id_to_inode: RwLock<BTreeMap<String, u64>>,
    /// file handle → open cache file (pread/pwrite — no full-file RAM load)
    open_files: RwLock<BTreeMap<u64, std::fs::File>>,
    /// file handles that have had write() called — uploaded on release()
    dirty_fhs: RwLock<HashSet<u64>>,
    fh_counter: AtomicU64,
    sync_dir: std::path::PathBuf,
    /// Local directory for caching downloaded on-demand files.
    /// Must be OUTSIDE the FUSE mountpoint to avoid recursive FUSE calls.
    cache_dir: std::path::PathBuf,
    /// OneDrive drive item ID of the root folder (parent_id of all top-level items).
    /// Stored in an RwLock so it can be refreshed after the initial delta sync populates the DB.
    root_drive_id: RwLock<String>,
    /// inode → SymlinkEntry for local-only symlinks
    symlink_inodes: RwLock<BTreeMap<u64, SymlinkEntry>>,
    /// (parent_inode, name) → symlink inode for fast lookup
    symlink_lookup: RwLock<BTreeMap<(u64, String), u64>>,
}

impl OneDriveFS {
    pub async fn new(
        db: Arc<Database>,
        graph: Arc<GraphClient>,
        sync_dir: std::path::PathBuf,
        cache_dir: std::path::PathBuf,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| anyhow::anyhow!("create cache dir {:?}: {e}", cache_dir))?;

        let root_drive_id = db
            .get_root_drive_id_sync(&sync_dir)
            .unwrap_or(None)
            .unwrap_or_default();

        Ok(Self {
            db,
            graph,
            inodes: RwLock::new(BTreeMap::new()),
            id_to_inode: RwLock::new(BTreeMap::new()),
            open_files: RwLock::new(BTreeMap::new()),
            dirty_fhs: RwLock::new(HashSet::new()),
            fh_counter: AtomicU64::new(1),
            sync_dir,
            cache_dir,
            root_drive_id: RwLock::new(root_drive_id),
            symlink_inodes: RwLock::new(BTreeMap::new()),
            symlink_lookup: RwLock::new(BTreeMap::new()),
        })
    }

    fn next_fh(&self) -> u64 {
        self.fh_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Returns the OneDrive item ID used as parent_id for children of `inode`.
    /// For FUSE root (inode 1) this is the drive root ID; otherwise looked up in the inode table.
    async fn drive_parent_id(&self, inode: u64) -> Option<String> {
        if inode == 1 {
            // Fast path: already known.
            {
                let id = self.root_drive_id.read().await;
                if !id.is_empty() {
                    return Some(id.clone());
                }
            }
            // Slow path: DB may now be populated (delta sync completed after mount).
            if let Ok(Some(id)) = self.db.get_root_drive_id(&self.sync_dir).await {
                if !id.is_empty() {
                    *self.root_drive_id.write().await = id.clone();
                    return Some(id);
                }
            }
            None
        } else {
            let map = self.inodes.read().await;
            map.get(&inode).map(|e| e.item_id.clone())
        }
    }

    async fn get_or_create_inode(&self, item_id: &str, parent_inode: u64, is_dir: bool) -> u64 {
        {
            let map = self.id_to_inode.read().await;
            if let Some(&ino) = map.get(item_id) {
                return ino;
            }
        }
        let ino = next_inode();
        let entry = InodeEntry {
            inode: ino,
            item_id: item_id.to_string(),
            parent_inode,
            is_dir,
        };
        self.inodes.write().await.insert(ino, entry);
        self.id_to_inode
            .write()
            .await
            .insert(item_id.to_string(), ino);
        ino
    }

    fn db_item_to_attr(&self, item: &DbItem, ino: u64) -> FileAttr {
        let kind = if item.is_folder {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let perm: u16 = if item.is_folder { 0o755 } else { 0o644 };

        let mtime = item
            .modified_at
            .map(|d| {
                let secs = d.timestamp().max(0) as u64;
                Timestamp::new(secs as i64, 0)
            })
            .unwrap_or_else(epoch_ts);

        let ctime = item
            .created_at
            .map(|d| {
                let secs = d.timestamp().max(0) as u64;
                Timestamp::new(secs as i64, 0)
            })
            .unwrap_or_else(epoch_ts);

        FileAttr {
            ino,
            size: item.size,
            blocks: (item.size + 511) / 512,
            atime: mtime,
            mtime,
            ctime,
            kind,
            perm,
            nlink: if item.is_folder { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }

    fn symlink_attr(&self, ino: u64, target_len: u64) -> FileAttr {
        let now = sys_time_to_ts(SystemTime::now());
        FileAttr {
            ino,
            size: target_len,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::Symlink,
            perm: 0o777,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }

    /// Resolve a parent inode to a local filesystem path (for symlink DB storage).
    async fn inode_to_path(&self, inode: u64) -> Option<std::path::PathBuf> {
        if inode == 1 {
            return Some(self.sync_dir.clone());
        }
        let map = self.inodes.read().await;
        let entry = map.get(&inode)?;
        let item = self.db.get_item_by_id(&entry.item_id).await.ok()??;
        Some(item.local_path)
    }

    fn root_attr(&self) -> FileAttr {
        let now = sys_time_to_ts(SystemTime::now());
        FileAttr {
            ino: 1,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }
}

impl Filesystem for OneDriveFS {
    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        Ok(ReplyInit {
            max_write: std::num::NonZeroU32::new(128 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {
        debug!("OneDriveFS: destroy");
    }

    async fn lookup(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        let name_str = name.to_string_lossy();
        debug!("lookup parent={parent} name={name_str}");

        let parent_drive_id = match self.drive_parent_id(parent).await {
            Some(id) => id,
            None => return Err(libc::ENOENT.into()),
        };

        if let Ok(Some(item)) = self.db.get_child_by_name(&parent_drive_id, &name_str).await {
            let ino = self
                .get_or_create_inode(&item.id, parent, item.is_folder)
                .await;
            let attr = self.db_item_to_attr(&item, ino);
            return Ok(ReplyEntry {
                ttl: std::time::Duration::from_secs(TTL_SEC),
                attr,
                generation: 0,
            });
        }

        // Check local-only symlinks.
        {
            let lookup = self.symlink_lookup.read().await;
            if let Some(&ino) = lookup.get(&(parent, name_str.to_string())) {
                let map = self.symlink_inodes.read().await;
                if let Some(entry) = map.get(&ino) {
                    let attr = self.symlink_attr(ino, entry.target.len() as u64);
                    return Ok(ReplyEntry {
                        ttl: std::time::Duration::from_secs(TTL_SEC),
                        attr,
                        generation: 0,
                    });
                }
            }
        }

        // Check DB for persisted symlinks not yet in memory (after daemon restart).
        if let Some(parent_path) = self.inode_to_path(parent).await {
            if let Ok(Some(target)) = self.db.get_symlink(&parent_path, &name_str).await {
                let ino = next_inode();
                let entry = SymlinkEntry {
                    inode: ino,
                    parent_inode: parent,
                    name: name_str.to_string(),
                    target: target.clone(),
                };
                self.symlink_inodes.write().await.insert(ino, entry);
                self.symlink_lookup
                    .write()
                    .await
                    .insert((parent, name_str.to_string()), ino);
                let attr = self.symlink_attr(ino, target.len() as u64);
                return Ok(ReplyEntry {
                    ttl: std::time::Duration::from_secs(TTL_SEC),
                    attr,
                    generation: 0,
                });
            }
        }

        Err(libc::ENOENT.into())
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> FuseResult<ReplyAttr> {
        debug!("getattr inode={inode}");
        if inode == 1 {
            return Ok(ReplyAttr {
                ttl: std::time::Duration::from_secs(TTL_SEC),
                attr: self.root_attr(),
            });
        }

        let item_id = {
            let map = self.inodes.read().await;
            map.get(&inode).map(|e| e.item_id.clone())
        };

        if let Some(id) = item_id {
            if let Ok(Some(item)) = self.db.get_item_by_id(&id).await {
                let attr = self.db_item_to_attr(&item, inode);
                return Ok(ReplyAttr {
                    ttl: std::time::Duration::from_secs(TTL_SEC),
                    attr,
                });
            }
        }

        // Check symlink inodes.
        {
            let map = self.symlink_inodes.read().await;
            if let Some(entry) = map.get(&inode) {
                let attr = self.symlink_attr(inode, entry.target.len() as u64);
                return Ok(ReplyAttr {
                    ttl: std::time::Duration::from_secs(TTL_SEC),
                    attr,
                });
            }
        }

        Err(libc::ENOENT.into())
    }

    // setattr: accept time/mode/uid/gid changes silently — we do not persist
    // them to OneDrive, but returning ENOSYS breaks tools like `touch`.
    async fn setattr(
        &self,
        _req: Request,
        inode: u64,
        fh: Option<u64>,
        set_attr: SetAttr,
    ) -> FuseResult<ReplyAttr> {
        // Handle truncation: editors (vim, nano, echo >) truncate before writing.
        // Without this, old bytes linger when the new content is shorter.
        if let Some(new_size) = set_attr.size {
            let item_id = {
                let map = self.inodes.read().await;
                map.get(&inode).map(|e| e.item_id.clone())
            };
            let item_id = item_id.ok_or(libc::ENOENT)?;
            let cache_path = self.cache_dir.join(&item_id);

            // Truncate the cache file.
            if cache_path.exists() {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&cache_path)
                    .map_err(|e| {
                        error!("truncate open {cache_path:?}: {e}");
                        libc::EIO
                    })?;
                file.set_len(new_size).map_err(|e| {
                    error!("truncate set_len {cache_path:?}: {e}");
                    libc::EIO
                })?;
            }

            // Mark the fh dirty so release() uploads the truncated file.
            if let Some(fh) = fh {
                self.dirty_fhs.write().await.insert(fh);
            }

            // Return updated attrs with new size.
            if let Ok(Some(item)) = self.db.get_item_by_id(&item_id).await {
                let mut attr = self.db_item_to_attr(&item, inode);
                attr.size = new_size;
                return Ok(ReplyAttr {
                    ttl: std::time::Duration::from_secs(TTL_SEC),
                    attr,
                });
            }
        }

        // For everything else (times, mode, uid, gid) just return current attrs.
        if inode == 1 {
            return Ok(ReplyAttr {
                ttl: std::time::Duration::from_secs(TTL_SEC),
                attr: self.root_attr(),
            });
        }

        let item_id = {
            let map = self.inodes.read().await;
            map.get(&inode).map(|e| e.item_id.clone())
        };

        if let Some(id) = item_id {
            if let Ok(Some(item)) = self.db.get_item_by_id(&id).await {
                let attr = self.db_item_to_attr(&item, inode);
                return Ok(ReplyAttr {
                    ttl: std::time::Duration::from_secs(TTL_SEC),
                    attr,
                });
            }
        }

        // Check symlink inodes.
        {
            let map = self.symlink_inodes.read().await;
            if let Some(entry) = map.get(&inode) {
                let attr = self.symlink_attr(inode, entry.target.len() as u64);
                return Ok(ReplyAttr {
                    ttl: std::time::Duration::from_secs(TTL_SEC),
                    attr,
                });
            }
        }

        Err(libc::ENOENT.into())
    }

    type DirEntryStream<'a> = stream::Iter<std::vec::IntoIter<FuseResult<DirectoryEntry>>>
    where
        Self: 'a;

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        inode: u64,
        _fh: u64,
        offset: i64,
    ) -> FuseResult<ReplyDirectory<Self::DirEntryStream<'a>>> {
        debug!("readdir inode={inode} offset={offset}");

        let parent_drive_id = match self.drive_parent_id(inode).await {
            Some(id) => id,
            None => return Err(libc::ENOENT.into()),
        };

        let children = self.db.get_children(&parent_drive_id).await.unwrap_or_default();
        let mut entries: Vec<FuseResult<DirectoryEntry>> = Vec::new();

        entries.push(Ok(DirectoryEntry {
            inode,
            offset: 1,
            kind: FileType::Directory,
            name: std::ffi::OsString::from("."),
        }));
        entries.push(Ok(DirectoryEntry {
            inode: 1,
            offset: 2,
            kind: FileType::Directory,
            name: std::ffi::OsString::from(".."),
        }));

        let mut entry_offset = 3i64;
        for item in &children {
            let ino = self
                .get_or_create_inode(&item.id, inode, item.is_folder)
                .await;
            let kind = if item.is_folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push(Ok(DirectoryEntry {
                inode: ino,
                offset: entry_offset,
                kind,
                name: std::ffi::OsString::from(&item.name),
            }));
            entry_offset += 1;
        }

        // Include local-only symlinks.
        if let Some(parent_path) = self.inode_to_path(inode).await {
            if let Ok(symlinks) = self.db.get_symlinks_in(&parent_path).await {
                for (name, target) in symlinks {
                    let lookup = self.symlink_lookup.read().await;
                    let ino = if let Some(&existing) = lookup.get(&(inode, name.clone())) {
                        existing
                    } else {
                        drop(lookup);
                        let new_ino = next_inode();
                        let entry = SymlinkEntry {
                            inode: new_ino,
                            parent_inode: inode,
                            name: name.clone(),
                            target,
                        };
                        self.symlink_inodes.write().await.insert(new_ino, entry);
                        self.symlink_lookup
                            .write()
                            .await
                            .insert((inode, name.clone()), new_ino);
                        new_ino
                    };
                    entries.push(Ok(DirectoryEntry {
                        inode: ino,
                        offset: entry_offset,
                        kind: FileType::Symlink,
                        name: std::ffi::OsString::from(&name),
                    }));
                    entry_offset += 1;
                }
            }
        }

        let skip = if offset == 0 { 0 } else { offset as usize };
        let result: Vec<_> = entries.into_iter().skip(skip).collect();

        Ok(ReplyDirectory {
            entries: stream::iter(result),
        })
    }

    type DirEntryPlusStream<'a> = stream::Iter<std::vec::IntoIter<FuseResult<DirectoryEntryPlus>>>
    where
        Self: 'a;

    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        inode: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<ReplyDirectoryPlus<Self::DirEntryPlusStream<'a>>> {
        debug!("readdirplus inode={inode} offset={offset}");

        let parent_drive_id = match self.drive_parent_id(inode).await {
            Some(id) => id,
            None => return Err(libc::ENOENT.into()),
        };

        let children = self.db.get_children(&parent_drive_id).await.unwrap_or_default();
        let mut entries: Vec<FuseResult<DirectoryEntryPlus>> = Vec::new();
        let ttl = std::time::Duration::from_secs(TTL_SEC);

        entries.push(Ok(DirectoryEntryPlus {
            inode,
            generation: 0,
            offset: 1,
            kind: FileType::Directory,
            name: std::ffi::OsString::from("."),
            attr: self.root_attr(),
            entry_ttl: ttl,
            attr_ttl: ttl,
        }));
        entries.push(Ok(DirectoryEntryPlus {
            inode: 1,
            generation: 0,
            offset: 2,
            kind: FileType::Directory,
            name: std::ffi::OsString::from(".."),
            attr: self.root_attr(),
            entry_ttl: ttl,
            attr_ttl: ttl,
        }));

        let mut entry_offset = 3i64;
        for item in &children {
            let ino = self
                .get_or_create_inode(&item.id, inode, item.is_folder)
                .await;
            let kind = if item.is_folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let attr = self.db_item_to_attr(item, ino);
            entries.push(Ok(DirectoryEntryPlus {
                inode: ino,
                generation: 0,
                offset: entry_offset,
                kind,
                name: std::ffi::OsString::from(&item.name),
                attr,
                entry_ttl: ttl,
                attr_ttl: ttl,
            }));
            entry_offset += 1;
        }

        // Include local-only symlinks.
        if let Some(parent_path) = self.inode_to_path(inode).await {
            if let Ok(symlinks) = self.db.get_symlinks_in(&parent_path).await {
                for (name, target) in symlinks {
                    let lookup = self.symlink_lookup.read().await;
                    let ino = if let Some(&existing) = lookup.get(&(inode, name.clone())) {
                        existing
                    } else {
                        drop(lookup);
                        let new_ino = next_inode();
                        let entry = SymlinkEntry {
                            inode: new_ino,
                            parent_inode: inode,
                            name: name.clone(),
                            target: target.clone(),
                        };
                        self.symlink_inodes.write().await.insert(new_ino, entry);
                        self.symlink_lookup
                            .write()
                            .await
                            .insert((inode, name.clone()), new_ino);
                        new_ino
                    };
                    let sym_attr = self.symlink_attr(ino, target.len() as u64);
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: ino,
                        generation: 0,
                        offset: entry_offset,
                        kind: FileType::Symlink,
                        name: std::ffi::OsString::from(&name),
                        attr: sym_attr,
                        entry_ttl: ttl,
                        attr_ttl: ttl,
                    }));
                    entry_offset += 1;
                }
            }
        }

        let skip = if offset == 0 { 0 } else { offset as usize };
        let result: Vec<_> = entries.into_iter().skip(skip).collect();

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(result),
        })
    }

    async fn open(&self, _req: Request, inode: u64, flags: u32) -> FuseResult<ReplyOpen> {
        debug!("open inode={inode} flags={flags:#o}");

        let item_id = {
            let map = self.inodes.read().await;
            map.get(&inode).map(|e| e.item_id.clone())
        };
        let item_id = item_id.ok_or(libc::ENOENT)?;

        let item = self
            .db
            .get_item_by_id(&item_id)
            .await
            .map_err(|_| libc::EIO)?
            .ok_or(libc::ENOENT)?;

        // Cache file lives OUTSIDE the FUSE mountpoint to avoid recursive FUSE calls.
        let cache_path = self.cache_dir.join(&item_id);

        // On-demand download — only if the cache file is missing or stale.
        // Skip for _local_* temp items — they exist only in cache, not on OneDrive.
        if !item_id.starts_with("_local_") && (item.is_placeholder || !cache_path.exists()) {
            debug!("open: on-demand download {item_id}");
            let graph = Arc::clone(&self.graph);
            let db = Arc::clone(&self.db);
            let id = item_id.clone();
            let path = cache_path.clone();

            // 30-second timeout so a hung download never freezes Dolphin.
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                graph.download_file(&id, &path),
            )
            .await
            {
                Ok(Ok(_)) => {
                    if let Err(e) = db.set_placeholder(&id, false).await {
                        warn!("Failed to clear placeholder for {id}: {e}");
                    }
                }
                Ok(Err(e)) => {
                    error!("on-demand download failed for {id}: {e}");
                    return Err(libc::EIO.into());
                }
                Err(_) => {
                    warn!("on-demand download timed out for {id}");
                    return Err(libc::EIO.into());
                }
            }
        }

        // Open the cache file — read+write so the same handle works for both
        // read() and write() FUSE calls without storing file content in RAM.
        let write_access = flags & libc::O_WRONLY as u32 != 0
            || flags & libc::O_RDWR as u32 != 0;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(write_access)
            .open(&cache_path)
            .map_err(|e| {
                error!("open cache {:?}: {e}", cache_path);
                libc::EIO
            })?;

        let fh = self.next_fh();
        self.open_files.write().await.insert(fh, file);
        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn read(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> FuseResult<ReplyData> {
        debug!("read fh={fh} offset={offset} size={size}");
        let files = self.open_files.read().await;
        let file = files.get(&fh).ok_or(libc::EBADF)?;

        // pread: reads at the given offset without seeking, safe for concurrent readers.
        let mut buf = vec![0u8; size as usize];
        let n = file.read_at(&mut buf, offset).map_err(|e| {
            error!("pread fh={fh} offset={offset}: {e}");
            libc::EIO
        })?;
        buf.truncate(n);
        Ok(ReplyData {
            data: bytes::Bytes::from(buf),
        })
    }

    async fn write(
        &self,
        _req: Request,
        inode: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        debug!("write inode={inode} fh={fh} offset={offset} len={}", data.len());

        // pwrite directly into the open cache file — no in-memory copy.
        {
            let files = self.open_files.read().await;
            let file = files.get(&fh).ok_or(libc::EBADF)?;
            file.write_at(data, offset).map_err(|e| {
                error!("pwrite fh={fh} offset={offset}: {e}");
                libc::EIO
            })?;
        }

        self.dirty_fhs.write().await.insert(fh);
        Ok(ReplyWrite {
            written: data.len() as u32,
        })
    }

    async fn release(
        &self,
        _req: Request,
        inode: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> FuseResult<()> {
        // Drop the file handle — file descriptor closes here.
        self.open_files.write().await.remove(&fh);
        let dirty = self.dirty_fhs.write().await.remove(&fh);

        if dirty {
            let item_id = {
                let map = self.inodes.read().await;
                map.get(&inode).map(|e| e.item_id.clone())
            };

            if let Some(id) = item_id {
                if let Ok(Some(item)) = self.db.get_item_by_id(&id).await {
                    let cache_path = self.cache_dir.join(&id);
                    let graph = Arc::clone(&self.graph);
                    let db = Arc::clone(&self.db);

                    // Upload in background — release() must return immediately so
                    // the kernel doesn't block the calling process (e.g. Dolphin).
                    tokio::spawn(async move {
                        if let Some(parent_id) = item.parent_id.clone() {
                            match graph.upload_file(&parent_id, &item.name, &cache_path).await {
                                Ok(updated) => {
                                    let new_size = updated.size.unwrap_or_else(|| {
                                        std::fs::metadata(&cache_path)
                                            .map(|m| m.len())
                                            .unwrap_or(0)
                                    });
                                    // Re-read from DB to get current name/local_path — a
                                    // concurrent rename() may have changed them while we
                                    // were uploading. Without this, we'd overwrite the
                                    // renamed entry with the stale pre-rename name.
                                    let current = db.get_item_by_id(&item.id).await
                                        .ok().flatten().unwrap_or(item);
                                    let mut updated_item = current;
                                    updated_item.size = new_size;
                                    updated_item.etag = updated.e_tag;
                                    updated_item.ctag = updated.c_tag;
                                    updated_item.modified_at = updated.last_modified_date_time;
                                    updated_item.is_placeholder = false;
                                    updated_item.sync_state = SyncState::Synced;
                                    if let Err(e) = db.upsert_item(&updated_item).await {
                                        error!("Failed to upsert item after upload: {e}");
                                    }
                                    info!(
                                        "upload complete: {} ({} bytes)",
                                        updated_item.name, new_size
                                    );
                                }
                                Err(e) => {
                                    error!("upload failed for {}: {e}", item.name);
                                }
                            }
                        }
                    });
                }
            }
        }

        Ok(())
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> FuseResult<ReplyEntry> {
        let name_str = name.to_string_lossy().to_string();
        debug!("mkdir parent={parent} name={name_str}");

        let parent_item_id = {
            let map = self.inodes.read().await;
            map.get(&parent).map(|e| e.item_id.clone())
        };

        let parent_id = match parent_item_id {
            Some(id) => id,
            None => {
                self.graph
                    .get_drive_root()
                    .await
                    .map_err(|e| {
                        error!("get_drive_root: {e}");
                        libc::EIO
                    })?
                    .id
            }
        };

        let folder = self
            .graph
            .create_folder(&parent_id, &name_str)
            .await
            .map_err(|e| {
                error!("create_folder: {e}");
                libc::EIO
            })?;

        // Build full local_path from parent's path, not just sync_dir + name.
        let parent_path = self.inode_to_path(parent).await
            .unwrap_or_else(|| self.sync_dir.clone());
        let local_path = parent_path.join(&name_str);
        let db_item = sync_engine::DbItem {
            id: folder.id.clone(),
            local_path: local_path.clone(),
            name: name_str,
            parent_id: Some(parent_id),
            etag: folder.e_tag,
            ctag: folder.c_tag,
            size: 0,
            modified_at: folder.last_modified_date_time,
            created_at: folder.created_date_time,
            sha1_hash: None,
            quick_xor_hash: None,
            is_folder: true,
            is_placeholder: false,
            sync_state: SyncState::Synced,
            pinned: false,
        };
        if let Err(e) = self.db.upsert_item(&db_item).await {
            error!("Failed to upsert folder: {e}");
        }
        let ino = self.get_or_create_inode(&folder.id, parent, true).await;
        let now_ts = Timestamp::from(SystemTime::now());

        Ok(ReplyEntry {
            ttl: std::time::Duration::from_secs(TTL_SEC),
            attr: FileAttr {
                ino,
                size: 0,
                blocks: 0,
                atime: now_ts,
                mtime: now_ts,
                ctime: now_ts,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                rdev: 0,
                blksize: 4096,
            },
            generation: 0,
        })
    }

    async fn unlink(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        let name_str = name.to_string_lossy();
        debug!("unlink parent={parent} name={name_str}");

        let parent_drive_id = match self.drive_parent_id(parent).await {
            Some(id) => id,
            None => return Err(libc::ENOENT.into()),
        };

        if let Ok(Some(item)) = self.db.get_child_by_name(&parent_drive_id, &name_str).await {
            // Skip Graph API call for local-only temp items.
            if !item.id.starts_with("_local_") {
                if let Err(e) = self.graph.delete_item(&item.id).await {
                    warn!("Failed to delete remote item {}: {e}", item.id);
                }
            }
            if let Err(e) = self.db.delete_item(&item.id).await {
                warn!("Failed to delete DB item {}: {e}", item.id);
            }
            // Remove the cache file if present. Do NOT touch item.local_path — it is
            // inside the FUSE mount and accessing it from the daemon causes a recursive
            // FUSE deadlock. The kernel removes the FUSE entry when we return Ok(()).
            let cache_path = self.cache_dir.join(&item.id);
            let _ = std::fs::remove_file(&cache_path);
            return Ok(());
        }

        // Check if it's a local-only symlink.
        {
            let lookup = self.symlink_lookup.read().await;
            if let Some(&ino) = lookup.get(&(parent, name_str.to_string())) {
                drop(lookup);
                self.symlink_inodes.write().await.remove(&ino);
                self.symlink_lookup
                    .write()
                    .await
                    .remove(&(parent, name_str.to_string()));
                if let Some(parent_path) = self.inode_to_path(parent).await {
                    let _ = self.db.delete_symlink(&parent_path, &name_str).await;
                }
                return Ok(());
            }
        }

        Err(libc::ENOENT.into())
    }

    async fn rename(
        &self,
        _req: Request,
        origin_parent: u64,
        origin_name: &OsStr,
        parent: u64,
        name: &OsStr,
    ) -> FuseResult<()> {
        let old_name = origin_name.to_string_lossy();
        let new_name = name.to_string_lossy();
        debug!("rename {old_name} -> {new_name}");

        let old_parent_drive_id = match self.drive_parent_id(origin_parent).await {
            Some(id) => id,
            None => return Err(libc::ENOENT.into()),
        };
        let new_parent_drive_id = match self.drive_parent_id(parent).await {
            Some(id) => id,
            None => return Err(libc::ENOENT.into()),
        };

        if let Ok(Some(item)) = self.db.get_child_by_name(&old_parent_drive_id, &old_name).await {
            let new_parent_path = self.inode_to_path(parent).await
                .unwrap_or_else(|| self.sync_dir.clone());
            let new_local_path = new_parent_path.join(new_name.as_ref());

            // If destination already exists, remove it first (POSIX rename semantics).
            if let Ok(Some(dest_item)) = self.db.get_child_by_name(&new_parent_drive_id, &new_name).await {
                if !dest_item.id.starts_with("_local_") {
                    let _ = self.graph.delete_item(&dest_item.id).await;
                }
                let _ = self.db.delete_item(&dest_item.id).await;
                let _ = std::fs::remove_file(self.cache_dir.join(&dest_item.id));
            }

            // Local-only temp items: rename locally without Graph API call.
            if item.id.starts_with("_local_") {
                let mut updated = item.clone();
                updated.name = new_name.to_string();
                updated.local_path = new_local_path;
                updated.parent_id = Some(new_parent_drive_id);
                let _ = self.db.delete_item(&item.id).await;
                if let Err(e) = self.db.upsert_item(&updated).await {
                    warn!("Failed to upsert renamed temp item: {e}");
                }
                return Ok(());
            }

            // Synced items: move on Graph API and update DB entry in place.
            match self
                .graph
                .move_item(&item.id, &new_parent_drive_id, &new_name)
                .await
            {
                Ok(moved) => {
                    // Update DB entry with new name/path — do NOT delete it,
                    // otherwise the file vanishes from FUSE until next delta sync.
                    let mut updated = item.clone();
                    updated.name = new_name.to_string();
                    updated.local_path = new_local_path;
                    updated.parent_id = Some(new_parent_drive_id);
                    updated.etag = moved.e_tag;
                    updated.ctag = moved.c_tag;
                    // Delete then re-insert (local_path changed, ON CONFLICT(id) handles it).
                    if let Err(e) = self.db.upsert_item(&updated).await {
                        warn!("Failed to update DB after rename: {e}");
                    }
                    return Ok(());
                }
                Err(e) => {
                    error!("rename failed: {e}");
                    return Err(libc::EIO.into());
                }
            }
        }
        Err(libc::ENOENT.into())
    }

    async fn getxattr(
        &self,
        _req: Request,
        inode: u64,
        name: &OsStr,
        size: u32,
    ) -> FuseResult<ReplyXAttr> {
        // Only serve our custom attribute; root inode has no sync state.
        if inode == 1 || name != OsStr::new("user.onedrive.syncstate") {
            return Err(libc::ENODATA.into());
        }

        // fuse3 bug (v0.7.3): ReplyXAttr::Size encodes error=+ERANGE in the reply header,
        // but the Linux FUSE driver rejects any reply with error > 0 as EINVAL, which
        // aborts the FUSE connection. Avoid ReplyXAttr::Size entirely.
        // Dolphin's KOverlayIconPlugin always passes a fixed 63-byte buffer (size > 0),
        // so it never triggers this path. Tools like getfattr do size=0 probes — return
        // ENODATA so they fail cleanly without crashing the FUSE session.
        if size == 0 {
            return Err(libc::ENODATA.into());
        }

        let item_id = {
            let map = self.inodes.read().await;
            map.get(&inode).map(|e| e.item_id.clone())
        };
        let item_id = item_id.ok_or(libc::ENODATA)?;

        let item = self
            .db
            .get_item_by_id(&item_id)
            .await
            .map_err(|_| libc::ENODATA)?
            .ok_or(libc::ENODATA)?;

        let state_str: &str = if item.pinned {
            "pinned"
        } else if item.is_folder {
            // Aggregate state: reflect whether descendants are in cloud or local.
            let agg = self
                .db
                .get_folder_aggregate_state(&item.local_path)
                .await
                .unwrap_or(SyncState::CloudOnly);
            match agg {
                SyncState::Pinned => "pinned",
                SyncState::Synced => "synced",
                SyncState::Partial => "partial",
                _ => "cloud",
            }
        } else {
            match &item.sync_state {
                SyncState::Synced => "synced",
                SyncState::Syncing => "syncing",
                SyncState::CloudOnly => "cloud",
                SyncState::Error(_) => "error",
                SyncState::LocalOnly => "local",
                SyncState::Conflict => "conflict",
                SyncState::Pinned => "pinned",
                // Partial is folder-aggregate only, never stored on individual files.
                SyncState::Partial => "synced",
            }
        };

        let bytes = state_str.as_bytes();
        if size < bytes.len() as u32 {
            return Err(libc::ERANGE.into());
        }

        Ok(ReplyXAttr::Data(bytes::Bytes::copy_from_slice(bytes)))
    }

    async fn create(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _flags: u32,
    ) -> FuseResult<ReplyCreated> {
        let name_str = name.to_string_lossy().to_string();
        debug!("create parent={parent} name={name_str}");

        let parent_id = {
            let map = self.inodes.read().await;
            match map.get(&parent).map(|e| e.item_id.clone()) {
                Some(id) => id,
                None => match self.drive_parent_id(parent).await {
                    Some(id) => id,
                    None => self
                        .graph
                        .get_drive_root()
                        .await
                        .map_err(|_| libc::EIO)?
                        .id,
                },
            }
        };

        // Build full local_path from parent's path.
        let parent_path = self.inode_to_path(parent).await
            .unwrap_or_else(|| self.sync_dir.clone());
        let local_path = parent_path.join(&name_str);

        // Local-first create: assign a temporary item ID, create the cache file
        // immediately, and upload in the background. This matches Mac OneDrive
        // behavior — files are instantly available after creation without waiting
        // for the Graph API round-trip.
        let tmp_item_id = format!("_local_{}", INODE_COUNTER.fetch_add(1, Ordering::SeqCst));
        let cache_path = self.cache_dir.join(&tmp_item_id);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&cache_path)
            .map_err(|e| {
                error!("create cache file {cache_path:?}: {e}");
                libc::EIO
            })?;

        // Insert a temporary DB entry so lookup/getattr/readdir find this file.
        let db_item = DbItem {
            id: tmp_item_id.clone(),
            local_path: local_path.clone(),
            name: name_str.clone(),
            parent_id: Some(parent_id.clone()),
            etag: None,
            ctag: None,
            size: 0,
            modified_at: Some(chrono::Utc::now()),
            created_at: Some(chrono::Utc::now()),
            sha1_hash: None,
            quick_xor_hash: None,
            is_folder: false,
            is_placeholder: false,
            sync_state: SyncState::Syncing,
            pinned: false,
        };
        if let Err(e) = self.db.upsert_item(&db_item).await {
            error!("Failed to upsert temp item on create: {e}");
        }

        let ino = self.get_or_create_inode(&tmp_item_id, parent, false).await;
        let fh = self.next_fh();
        self.open_files.write().await.insert(fh, file);
        // Mark dirty so release() uploads the file content.
        self.dirty_fhs.write().await.insert(fh);
        let now_ts = Timestamp::from(SystemTime::now());

        Ok(ReplyCreated {
            ttl: std::time::Duration::from_secs(TTL_SEC),
            attr: FileAttr {
                ino,
                size: 0,
                blocks: 0,
                atime: now_ts,
                mtime: now_ts,
                ctime: now_ts,
                kind: FileType::RegularFile,
                perm: 0o644,
                nlink: 1,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                rdev: 0,
                blksize: 4096,
            },
            generation: 0,
            fh,
            flags: 0,
        })
    }

    async fn statfs(&self, _req: Request, _inode: u64) -> FuseResult<ReplyStatFs> {
        // Return reasonable defaults. Real quota info would require a Graph API
        // call which is too expensive for every statfs invocation.
        Ok(ReplyStatFs {
            blocks: 1_000_000_000 / 4, // ~1 TB in 4K blocks
            bfree: 500_000_000 / 4,    // ~500 GB free
            bavail: 500_000_000 / 4,
            files: 1_000_000,
            ffree: 500_000,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
    }

    async fn symlink(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        link: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        let name_str = name.to_string_lossy().to_string();
        let target = link.to_string_lossy().to_string();
        debug!("symlink parent={parent} name={name_str} -> {target}");

        // Store in DB for persistence across daemon restarts.
        let parent_path = self.inode_to_path(parent).await.ok_or(libc::ENOENT)?;
        self.db
            .create_symlink(&parent_path, &name_str, &target)
            .await
            .map_err(|e| {
                error!("create_symlink DB error: {e}");
                libc::EIO
            })?;

        // Create inode for the symlink.
        let ino = next_inode();
        let entry = SymlinkEntry {
            inode: ino,
            parent_inode: parent,
            name: name_str.clone(),
            target: target.clone(),
        };
        self.symlink_inodes.write().await.insert(ino, entry);
        self.symlink_lookup
            .write()
            .await
            .insert((parent, name_str), ino);

        let attr = self.symlink_attr(ino, target.len() as u64);
        Ok(ReplyEntry {
            ttl: std::time::Duration::from_secs(TTL_SEC),
            attr,
            generation: 0,
        })
    }

    async fn readlink(&self, _req: Request, inode: u64) -> FuseResult<ReplyData> {
        debug!("readlink inode={inode}");
        let map = self.symlink_inodes.read().await;
        let entry = map.get(&inode).ok_or(libc::ENOENT)?;
        Ok(ReplyData {
            data: bytes::Bytes::from(entry.target.clone().into_bytes()),
        })
    }
}
