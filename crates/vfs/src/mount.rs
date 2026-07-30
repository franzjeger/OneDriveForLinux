use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

/// Ensure the mountpoint directory exists and is empty.
/// Removes any stale real files left by a previous non-FUSE run.
pub fn prepare_mountpoint(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path).with_context(|| format!("create mountpoint {path:?}"))?;
        info!("Created mountpoint {:?}", path);
        return Ok(());
    }

    // Real files/dirs here block FUSE from mounting. They can be left over if
    // the daemon previously ran without on_demand mode — in which case they are
    // the user's synced files, possibly with local-only edits. Move them aside
    // instead of deleting so nothing is ever lost.
    let entries: Vec<_> = std::fs::read_dir(path)
        .with_context(|| format!("read mountpoint {path:?}"))?
        .filter_map(|e| e.ok())
        .collect();

    if !entries.is_empty() {
        let backup = backup_dir_for(path);
        std::fs::create_dir_all(&backup)
            .with_context(|| format!("create backup dir {backup:?}"))?;
        info!(
            "Mountpoint {:?} has {} existing entries — moving them to {:?}",
            path,
            entries.len(),
            backup
        );
        for entry in entries {
            let src = entry.path();
            let dest = backup.join(entry.file_name());
            std::fs::rename(&src, &dest).with_context(|| format!("move {src:?} to {dest:?}"))?;
        }
    }

    Ok(())
}

/// Sibling directory the pre-FUSE contents are moved into, e.g.
/// `~/OneDrive` → `~/OneDrive.pre-fuse-backup` (numbered if already present).
fn backup_dir_for(path: &Path) -> std::path::PathBuf {
    let base = path.with_extension("pre-fuse-backup");
    if !base.exists() {
        return base;
    }
    for i in 1.. {
        let candidate = path.with_extension(format!("pre-fuse-backup.{i}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Check whether a path is currently a FUSE mount.
pub fn is_mounted(path: &Path) -> bool {
    // Read /proc/mounts and look for our mountpoint
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let target = path.to_string_lossy();
    mounts.lines().any(|line| {
        let mut parts = line.split_whitespace();
        parts.next(); // device
        parts
            .next()
            .map(|mp| mp == target.as_ref())
            .unwrap_or(false)
    })
}

/// Unmount a FUSE filesystem using `fusermount3 -u` or `umount`.
pub fn unmount(path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();

    // Try fusermount3 -uz first (lazy unmount — safe even if mount is busy or daemon is dead)
    let status = std::process::Command::new("fusermount3")
        .args(["-uz", &path_str])
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("Unmounted {:?} via fusermount3 -uz", path);
            return Ok(());
        }
        _ => {}
    }

    // Fall back to umount -l (lazy)
    let status = std::process::Command::new("umount")
        .args(["-l", &path_str])
        .status()
        .context("run umount -l")?;

    if status.success() {
        info!("Unmounted {:?} via umount -l", path);
        Ok(())
    } else {
        anyhow::bail!("Failed to unmount {:?}", path);
    }
}
