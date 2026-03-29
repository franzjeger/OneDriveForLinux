mod dbus;

use anyhow::{Context, Result};
use futures::StreamExt;
use fuse3::{raw::Session, MountOptions};
use graph_client::{AuthManager, GraphClient};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook_tokio::Signals;
use std::{path::PathBuf, sync::Arc};
use sync_engine::{Config, Database, SyncEngine};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Tracing ────────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .init();

    // ── Panic hook — ensure FUSE is unmounted on crash ─────────────────────────
    // Without this, a panic leaves a ghost FUSE mount that hangs any process
    // (e.g. Dolphin) that tries to access the sync directory.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        // Best-effort lazy unmount; ignore errors.
        let sync_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/root"))
            .join("OneDrive");
        let _ = std::process::Command::new("fusermount3")
            .args(["-uz", &sync_dir.to_string_lossy().to_string()])
            .status();
    }));

    info!("OneDrive for Linux daemon starting");

    // ── Single-instance lock ───────────────────────────────────────────────────
    let pid_path = dirs::runtime_dir()
        .unwrap_or_else(|| dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")))
        .join("onedrive-linux.pid");
    if let Ok(existing) = std::fs::read_to_string(&pid_path) {
        let existing_pid = existing.trim().parse::<u32>().unwrap_or(0);
        let stat_path = PathBuf::from(format!("/proc/{existing_pid}/stat"));
        let is_alive = std::fs::read_to_string(&stat_path)
            .map(|s| !s.contains(") Z"))  // Z = zombie, not really running
            .unwrap_or(false);
        if is_alive {
            anyhow::bail!(
                "Another instance is already running (PID {existing_pid}). \
                 Stop it first with: kill {existing_pid}"
            );
        }
    }
    std::fs::write(&pid_path, std::process::id().to_string()).ok();

    // ── First-run setup ────────────────────────────────────────────────────────
    // If no config file exists yet, run the automatic admin setup flow.
    // This opens a browser for a Global Admin to log in, creates an Azure app
    // registration via Graph API, and writes config.toml before continuing.
    let config_path = sync_engine::config::config_path();
    if !config_path.exists() {
        println!("First run: no config found at {}.", config_path.display());
        println!("Starting automatic setup — a browser window will open.");
        graph_client::setup::AdminSetup::run(&config_path)
            .await
            .context("first-run admin setup")?;
        println!("Setup complete. Starting sync…");
    }

    // ── Config ─────────────────────────────────────────────────────────────────
    let config = Config::load().context("load config")?;
    let config = Arc::new(config);
    info!("Sync directory: {:?}", config.sync_dir);

    // Unmount any stale FUSE mount left by a previous unclean shutdown.
    if vfs::is_mounted(&config.sync_dir) {
        info!("Unmounting stale FUSE mount at {:?}", config.sync_dir);
        let _ = vfs::unmount(&config.sync_dir);
    }
    tokio::fs::create_dir_all(&config.sync_dir)
        .await
        .context("create sync_dir")?;

    // ── Auth ───────────────────────────────────────────────────────────────────
    let auth = Arc::new(
        AuthManager::new(config.client_id.clone(), config.tenant_id.clone())
            .context("init auth manager")?,
    );

    if !auth.is_authenticated().await {
        info!("No saved tokens — starting device code flow");
        auth.authenticate_device_code()
            .await
            .context("device code authentication")?;
    }

    // ── Database ───────────────────────────────────────────────────────────────
    let db_path = db_path();
    let db = Arc::new(Database::open(&db_path).context("open database")?);
    info!("Database: {:?}", db_path);

    // ── Graph client ───────────────────────────────────────────────────────────
    let graph = Arc::new(GraphClient::new(Arc::clone(&auth)));

    // ── Sync engine ────────────────────────────────────────────────────────────
    let engine_cache_dir = if config.on_demand {
        Some(
            dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("onedrive-linux")
                .join("files"),
        )
    } else {
        None
    };

    let (engine, event_rx) = SyncEngine::new(
        Arc::clone(&config),
        Arc::clone(&db),
        Arc::clone(&graph),
        engine_cache_dir,
    );
    let engine = Arc::new(engine);

    Arc::clone(&engine)
        .start()
        .await
        .context("start sync engine")?;

    // ── FUSE VFS ───────────────────────────────────────────────────────────────
    let mount_handle = if config.on_demand {
        let mountpoint = config.sync_dir.clone();
        vfs::prepare_mountpoint(&mountpoint).context("prepare mountpoint")?;

        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("onedrive-linux")
            .join("files");

        let fs = vfs::OneDriveFS::new(
            Arc::clone(&db),
            Arc::clone(&graph),
            mountpoint.clone(),
            cache_dir,
        )
        .await
        .context("create FUSE filesystem")?;

        let mount_options = MountOptions::default();
        let handle = Session::new(mount_options)
            .mount_with_unprivileged(fs, &mountpoint)
            .await
            .context("mount FUSE filesystem")?;

        info!("FUSE filesystem mounted at {:?}", mountpoint);
        Some(handle)
    } else {
        None
    };

    // ── System tray ────────────────────────────────────────────────────────────
    let config_path = sync_engine::config::config_path();
    if let Err(e) = tray::spawn_tray(config.sync_dir.clone(), config_path, event_rx) {
        error!("Tray failed to start (OK in headless environments): {e}");
    }

    // ── D-Bus ──────────────────────────────────────────────────────────────────
    let _dbus_conn = match dbus::export_dbus(Arc::clone(&engine)).await {
        Ok(conn) => Some(conn),
        Err(e) => {
            error!("D-Bus interface unavailable (odctl won't work): {e}");
            None
        }
    };

    // ── Signal handling ────────────────────────────────────────────────────────
    let mut signals = Signals::new([SIGTERM, SIGINT]).context("register signals")?;
    info!("Daemon running — waiting for signals");

    while let Some(signal) = signals.next().await {
        match signal {
            SIGTERM | SIGINT => {
                info!("Received signal {signal} — shutting down");
                break;
            }
            _ => {}
        }
    }

    // ── Graceful shutdown ──────────────────────────────────────────────────────
    if let Some(handle) = mount_handle {
        info!("Unmounting FUSE...");
        if let Err(e) = handle.unmount().await {
            error!("Unmount error: {e}");
        }
    }

    info!("Daemon stopped");
    Ok(())
}

fn db_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/root/.local/share"))
        .join("onedrive-linux")
        .join("sync.db")
}
