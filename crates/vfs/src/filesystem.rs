use fuse3::{
    raw::{
        reply::{
            DirectoryEntry, DirectoryEntryPlus, FileAttr, ReplyAttr, ReplyCreated, ReplyData,
            ReplyDirectory, ReplyDirectoryPlus, ReplyEntry, ReplyInit, ReplyOpen, ReplyWrite,
            ReplyXAttr,
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

        Err(libc::ENOENT.into())
    }

    // setattr: accept time/mode/uid/gid changes silently — we do not persist
    // them to OneDrive, but returning ENOSYS breaks tools like `touch`.
    // Size changes (truncation) are not supported on the FUSE layer.
    async fn setattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> FuseResult<ReplyAttr> {
        // Reject truncation — we don't support in-place writes via FUSE.
        if set_attr.size.is_some() {
            return Err(libc::EPERM.into());
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
        if item.is_placeholder || !cache_path.exists() {
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
                                    let mut updated_item = item;
                                    updated_item.size = updated.size.unwrap_or_else(|| {
                                        std::fs::metadata(&cache_path)
                                            .map(|m| m.len())
                                            .unwrap_or(0)
                                    });
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
                                        updated_item.name, updated_item.size
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

        let local_path = self.sync_dir.join(&name_str);
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
            if let Err(e) = self.graph.delete_item(&item.id).await {
                warn!("Failed to delete remote item {}: {e}", item.id);
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
            match self
                .graph
                .move_item(&item.id, &new_parent_drive_id, &new_name)
                .await
            {
                Ok(_) => {
                    if let Err(e) = self.db.delete_item(&item.id).await {
                        warn!("Failed to delete DB item after rename: {e}");
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
                None => self
                    .graph
                    .get_drive_root()
                    .await
                    .map_err(|_| libc::EIO)?
                    .id,
            }
        };

        // Create an empty file in the cache dir (outside the FUSE mount) and upload
        // from there. Creating at local_path (inside the FUSE mount) from within the
        // FUSE handler would cause a recursive FUSE deadlock.
        let tmp_path = self.cache_dir.join(format!("new_{name_str}"));
        std::fs::File::create(&tmp_path).map_err(|_| libc::EIO)?;
        let local_path = self.sync_dir.join(&name_str);

        let item = self
            .graph
            .upload_file(&parent_id, &name_str, &tmp_path)
            .await
            .map_err(|e| {
                error!("upload on create: {e}");
                libc::EIO
            })?;

        let db_item = DbItem {
            id: item.id.clone(),
            local_path: local_path.clone(),
            name: name_str,
            parent_id: Some(parent_id),
            etag: item.e_tag.clone(),
            ctag: item.c_tag.clone(),
            size: 0,
            modified_at: item.last_modified_date_time,
            created_at: item.created_date_time,
            sha1_hash: None,
            quick_xor_hash: None,
            is_folder: false,
            is_placeholder: false,
            sync_state: SyncState::Synced,
            pinned: false,
        };
        if let Err(e) = self.db.upsert_item(&db_item).await {
            error!("Failed to upsert item on create: {e}");
        }

        // Move tmp file to the canonical cache path now that we have the item id.
        let cache_path = self.cache_dir.join(&item.id);
        if let Err(e) = std::fs::rename(&tmp_path, &cache_path) {
            warn!("Failed to rename tmp cache file: {e}");
        }

        let ino = self.get_or_create_inode(&item.id, parent, false).await;
        let fh = self.next_fh();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&cache_path)
            .map_err(|e| { error!("open cache on create: {e}"); libc::EIO })?;
        self.open_files.write().await.insert(fh, file);
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
}
