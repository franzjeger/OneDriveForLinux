use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tabled::{Table, Tabled};
use zbus::{proxy, Connection};

// ── D-Bus proxy ────────────────────────────────────────────────────────────────

#[proxy(
    interface = "com.onedrive.linux1",
    default_service = "com.onedrive.linux1",
    default_path = "/com/onedrive/linux1"
)]
trait OneDriveControl {
    async fn pause(&self) -> zbus::Result<()>;
    async fn resume(&self) -> zbus::Result<()>;
    async fn get_status(&self) -> zbus::Result<Vec<(String, String)>>;
    async fn force_sync(&self, path: String) -> zbus::Result<()>;
    async fn is_paused(&self) -> zbus::Result<bool>;
    async fn pin_item(&self, path: String) -> zbus::Result<()>;
    async fn unpin_item(&self, path: String) -> zbus::Result<()>;
}

// ── CLI definition ─────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "odctl",
    about = "Control the OneDrive for Linux daemon",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show sync status of all tracked files
    Status,

    /// Pause sync
    Pause,

    /// Resume sync
    Resume,

    /// Force sync a specific path (or sync dir if omitted)
    Sync {
        /// Path to force-sync (relative or absolute)
        path: Option<String>,
    },

    /// Show current configuration
    Config,

    /// Authentication management
    Auth {
        /// Sign out and remove saved tokens
        #[arg(long)]
        signout: bool,
    },

    /// Show download status of pinned files (are they actually on device?)
    PinStatus {
        /// Optional path filter (show only pins under this path)
        path: Option<String>,
    },

    /// Keep file(s)/folder(s) always on device (download now, never evict)
    Pin {
        /// One or more paths to pin (files or folders)
        #[arg(required = true)]
        paths: Vec<String>,
    },

    /// Free up space — remove from device, keep in cloud only
    Unpin {
        /// One or more paths to unpin (files or folders)
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

// ── Table row for status output ────────────────────────────────────────────────

#[derive(Tabled)]
struct StatusRow {
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "State")]
    state: String,
}

// ── Main ───────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Status => cmd_status().await,
        Command::PinStatus { path } => cmd_pin_status(path).await,
        Command::Pause => cmd_pause().await,
        Command::Resume => cmd_resume().await,
        Command::Sync { path } => cmd_sync(path).await,
        Command::Config => cmd_config(),
        Command::Auth { signout } => cmd_auth(signout).await,
        Command::Pin { paths } => cmd_pin(paths).await,
        Command::Unpin { paths } => cmd_unpin(paths).await,
    }
}

async fn make_proxy(conn: &Connection) -> Result<OneDriveControlProxy<'_>> {
    OneDriveControlProxy::new(conn)
        .await
        .context("connect to daemon via D-Bus — is onedrive-daemon running?")
}

async fn cmd_status() -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus session bus")?;
    let proxy = make_proxy(&conn).await?;

    let items = proxy.get_status().await.context("get_status D-Bus call")?;
    let paused = proxy.is_paused().await.unwrap_or(false);

    if paused {
        println!("[PAUSED]");
    }

    if items.is_empty() {
        println!("No items tracked yet.");
        return Ok(());
    }

    let rows: Vec<StatusRow> = items
        .into_iter()
        .map(|(path, state)| StatusRow { path, state })
        .collect();

    println!("{}", Table::new(rows));
    Ok(())
}

async fn cmd_pause() -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;
    proxy.pause().await.context("pause D-Bus call")?;
    println!("Sync paused.");
    Ok(())
}

async fn cmd_resume() -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;
    proxy.resume().await.context("resume D-Bus call")?;
    println!("Sync resumed.");
    Ok(())
}

async fn cmd_sync(path: Option<String>) -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;

    let target = match path {
        Some(p) => std::fs::canonicalize(&p)
            .unwrap_or_else(|_| std::path::PathBuf::from(&p))
            .to_string_lossy()
            .to_string(),
        None => {
            // Read sync_dir from config file
            let cfg_path = config_path();
            let raw = std::fs::read_to_string(&cfg_path)
                .with_context(|| format!("read config {cfg_path:?}"))?;
            let val: toml::Value = toml::from_str(&raw).context("parse config")?;
            val.get("sync_dir")
                .and_then(|v| v.as_str())
                .unwrap_or("~/OneDrive")
                .to_string()
        }
    };

    proxy.force_sync(target.clone()).await.context("force_sync D-Bus call")?;
    println!("Force sync requested for: {target}");
    Ok(())
}

async fn cmd_pin(paths: Vec<String>) -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;
    for path in paths {
        let abs = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&path))
            .to_string_lossy()
            .to_string();
        proxy.pin_item(abs.clone()).await.context("pin_item D-Bus call")?;
        println!("Pinning: {abs} (downloading in background)");
    }
    Ok(())
}

async fn cmd_unpin(paths: Vec<String>) -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;
    for path in paths {
        let abs = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&path))
            .to_string_lossy()
            .to_string();
        proxy.unpin_item(abs.clone()).await.context("unpin_item D-Bus call")?;
        println!("Unpinned: {abs} (freed from device)");
    }
    Ok(())
}

fn cmd_config() -> Result<()> {
    let cfg_path = config_path();
    if !cfg_path.exists() {
        eprintln!("Config file not found at {cfg_path:?}");
        eprintln!(
            "\nCreate it with at minimum:\n  client_id = \"<your-azure-app-client-id>\"\n"
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read {cfg_path:?}"))?;
    println!("# Config: {cfg_path:?}\n\n{raw}");
    Ok(())
}

async fn cmd_auth(signout: bool) -> Result<()> {
    if signout {
        let tokens_path = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/root/.config"))
            .join("onedrive-linux")
            .join("tokens.json");

        if tokens_path.exists() {
            tokio::fs::remove_file(&tokens_path)
                .await
                .context("remove tokens.json")?;
            println!("Signed out — tokens removed.");
            println!("Restart the daemon to re-authenticate.");
        } else {
            println!("Not signed in (no tokens.json found).");
        }
    } else {
        let tokens_path = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/root/.config"))
            .join("onedrive-linux")
            .join("tokens.json");
        println!("To re-authenticate, stop the daemon and delete:");
        println!("  {tokens_path:?}");
        println!("Then restart the daemon — the device code flow will start automatically.");
    }
    Ok(())
}

async fn cmd_pin_status(path_filter: Option<String>) -> Result<()> {
    let db_path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/root/.local/share"))
        .join("onedrive-linux")
        .join("sync.db");
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("onedrive-linux")
        .join("files");

    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open database {db_path:?}"))?;

    let query = if path_filter.is_some() {
        "SELECT id, local_path, name, size, is_placeholder FROM items WHERE pinned = 1 AND is_folder = 0 AND local_path LIKE ?1 ORDER BY local_path"
    } else {
        "SELECT id, local_path, name, size, is_placeholder FROM items WHERE pinned = 1 AND is_folder = 0 ORDER BY local_path"
    };

    let mut stmt = conn.prepare(query)?;
    let rows: Vec<(String, String, String, i64, bool)> = if let Some(ref p) = path_filter {
        let abs = std::fs::canonicalize(p)
            .unwrap_or_else(|_| std::path::PathBuf::from(p));
        let like = format!("{}%", abs.to_string_lossy());
        stmt.query_map(rusqlite::params![like], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get::<_, i32>(4)? != 0))
        })?.filter_map(|r| r.ok()).collect()
    } else {
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get::<_, i32>(4)? != 0))
        })?.filter_map(|r| r.ok()).collect()
    };

    if rows.is_empty() {
        println!("No pinned files found.");
        return Ok(());
    }

    let mut on_device = 0u64;
    let mut missing = 0u64;
    let mut total_bytes = 0u64;
    let mut cached_bytes = 0u64;
    let mut missing_files: Vec<String> = Vec::new();

    for (id, local_path, _name, size, _is_placeholder) in &rows {
        total_bytes += *size as u64;
        let cache_path = cache_dir.join(id);
        if cache_path.exists() {
            on_device += 1;
            cached_bytes += std::fs::metadata(&cache_path).map(|m| m.len()).unwrap_or(0);
        } else {
            missing += 1;
            missing_files.push(local_path.clone());
        }
    }

    let total = rows.len() as u64;
    println!("Pinned files: {total}");
    println!("On device:    {on_device}/{total} ({:.0}%)", on_device as f64 / total as f64 * 100.0);
    println!("Missing:      {missing}");
    println!("Cached size:  {}", format_bytes(cached_bytes));
    println!("Expected:     {}", format_bytes(total_bytes));

    if !missing_files.is_empty() {
        println!("\nMissing files:");
        for f in &missing_files {
            println!("  {f}");
        }
    } else {
        println!("\nAll pinned files are on device.");
    }

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/root/.config"))
        .join("onedrive-linux")
        .join("config.toml")
}
