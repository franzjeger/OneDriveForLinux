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
    async fn get_state_counts(&self) -> zbus::Result<Vec<(String, u32)>>;
    async fn get_attention_items(&self, limit: u32) -> zbus::Result<Vec<(String, String)>>;
    async fn force_sync(&self, path: String) -> zbus::Result<()>;
    async fn is_paused(&self) -> zbus::Result<bool>;
    async fn pin_item(&self, path: String) -> zbus::Result<()>;
    async fn unpin_item(&self, path: String) -> zbus::Result<()>;
    async fn start_auth(&self) -> zbus::Result<(String, String, String)>;
}

// ── CLI definition ─────────────────────────────────────────────────────────────

/// The release this binary was built from.
///
/// Every crate in the workspace is version 0.1.0 and always has been — the
/// release number lives only in the git tag, so `--version` reported "0.1.0"
/// for every build ever shipped. That is useless for the one question it gets
/// asked: did my upgrade actually take? The release workflow sets
/// ONEDRIVE_RELEASE_VERSION; a build without it is not a release.
pub const VERSION: &str = match option_env!("ONEDRIVE_RELEASE_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), " (development build)"),
};

#[derive(Debug, Parser)]
#[command(
    name = "odctl",
    about = "Control the OneDrive for Linux daemon",
    version = VERSION
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show sync status (summary; use --all for every tracked file)
    Status {
        /// List every tracked file instead of the summary
        #[arg(long)]
        all: bool,
    },

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
        Command::Status { all } => cmd_status(all).await,
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

/// ANSI styling that steps aside when piped or when NO_COLOR is set.
struct Style {
    on: bool,
}

impl Style {
    fn detect() -> Self {
        use std::io::IsTerminal;
        Self {
            on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn good(&self, t: &str) -> String {
        self.paint("32", t)
    }
    fn accent(&self, t: &str) -> String {
        self.paint("34", t)
    }
    fn warn(&self, t: &str) -> String {
        self.paint("33", t)
    }
    fn bad(&self, t: &str) -> String {
        self.paint("31", t)
    }
    fn dim(&self, t: &str) -> String {
        self.paint("2", t)
    }
    fn bold(&self, t: &str) -> String {
        self.paint("1", t)
    }
}

async fn cmd_status(all: bool) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connect to D-Bus session bus")?;
    let proxy = make_proxy(&conn).await?;

    let paused = proxy.is_paused().await.unwrap_or(false);
    let style = Style::detect();

    // --all is the only mode that genuinely needs every row. The summary asks
    // the daemon to count in SQL instead: on a large drive the full table is
    // hundreds of thousands of entries, and shipping it over the bus to derive
    // six numbers is pure waste.
    if all {
        let items = proxy.get_status().await.context("get_status D-Bus call")?;
        if items.is_empty() {
            println!("No items tracked yet.");
            return Ok(());
        }
        if paused {
            println!("{}", style.warn("[PAUSED]"));
        }
        let rows: Vec<StatusRow> = items
            .into_iter()
            .map(|(path, state)| StatusRow { path, state })
            .collect();
        println!("{}", Table::new(rows));
        return Ok(());
    }

    let counts = proxy
        .get_state_counts()
        .await
        .context("get_state_counts D-Bus call")?;
    let total: u32 = counts.iter().map(|(_, n)| n).sum();
    if total == 0 {
        println!("No items tracked yet.");
        return Ok(());
    }
    let count_of = |name: &str| -> u32 {
        counts
            .iter()
            .find(|(state, _)| state == name)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    // Names are the stored state strings, which are stable, unlike the
    // human-readable text they are rendered as.
    let synced = count_of("synced") + count_of("partial");
    let cloud = count_of("cloud_only");
    let pinned = count_of("pinned");
    let local = count_of("local_only");
    let syncing_count = count_of("syncing");
    let error_count = count_of("error") + count_of("conflict");

    // Only the handful actually shown are fetched.
    let attention = proxy.get_attention_items(10).await.unwrap_or_default();
    let syncing: Vec<&str> = attention
        .iter()
        .filter(|(_, state)| state == "Syncing")
        .map(|(path, _)| path.as_str())
        .collect();
    let errors: Vec<(&str, &str)> = attention
        .iter()
        .filter(|(_, state)| state != "Syncing")
        .map(|(path, state)| (path.as_str(), state.as_str()))
        .collect();

    let headline = if paused {
        style.warn("⏸ paused")
    } else if error_count > 0 {
        style.bad("● needs attention")
    } else if syncing_count > 0 {
        style.accent("↻ syncing")
    } else {
        style.good("● up to date")
    };
    println!(
        "{}  {} {}",
        style.bold("OneDrive"),
        headline,
        style.dim(&format!("· {total} items tracked"))
    );
    println!();
    println!(
        "  {}   {}   {}   {}   {}",
        style.good(&format!("✓ {synced} synced")),
        style.warn(&format!("● {pinned} pinned")),
        style.dim(&format!("○ {cloud} cloud-only")),
        style.accent(&format!("↻ {syncing_count} syncing")),
        if error_count == 0 {
            style.dim("✗ 0 errors")
        } else {
            style.bad(&format!("✗ {error_count} errors"))
        },
    );
    if local > 0 {
        println!("  {}", style.dim(&format!("↑ {local} awaiting upload")));
    }

    if !syncing.is_empty() {
        println!();
        for path in syncing.iter().take(5) {
            println!("  {} {path}", style.accent("↻"));
        }
        if syncing_count as usize > syncing.len().min(5) {
            println!(
                "  {}",
                style.dim(&format!(
                    "… and {} more",
                    syncing_count as usize - syncing.len().min(5)
                ))
            );
        }
    }
    if !errors.is_empty() {
        println!();
        for (path, state) in errors.iter().take(5) {
            println!("  {} {path} {}", style.bad("✗"), style.dim(state));
        }
    }

    println!();
    println!(
        "{}",
        style.dim("Run `odctl status --all` for the full table.")
    );
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
        Some(p) => absolutize(&p),
        None => {
            // Read sync_dir from config file
            let cfg_path = config_path();
            let raw = std::fs::read_to_string(&cfg_path)
                .with_context(|| format!("read config {cfg_path:?}"))?;
            let val: toml::Value = toml::from_str(&raw).context("parse config")?;
            match val.get("sync_dir").and_then(|v| v.as_str()) {
                Some(dir) => dir.to_string(),
                None => dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/root"))
                    .join("OneDrive")
                    .to_string_lossy()
                    .to_string(),
            }
        }
    };

    proxy
        .force_sync(target.clone())
        .await
        .context("force_sync D-Bus call")?;
    println!("Force sync requested for: {target}");
    Ok(())
}

async fn cmd_pin(paths: Vec<String>) -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;
    for path in paths {
        let abs = absolutize(&path);
        proxy
            .pin_item(abs.clone())
            .await
            .context("pin_item D-Bus call")?;
        println!("Pinning: {abs} (downloading in background)");
    }
    Ok(())
}

async fn cmd_unpin(paths: Vec<String>) -> Result<()> {
    let conn = Connection::session().await.context("connect to D-Bus")?;
    let proxy = make_proxy(&conn).await?;
    for path in paths {
        let abs = absolutize(&path);
        proxy
            .unpin_item(abs.clone())
            .await
            .context("unpin_item D-Bus call")?;
        println!("Unpinned: {abs} (freed from device)");
    }
    Ok(())
}

fn cmd_config() -> Result<()> {
    let cfg_path = config_path();
    if !cfg_path.exists() {
        eprintln!("Config file not found at {cfg_path:?}");
        eprintln!("\nCreate it with at minimum:\n  client_id = \"<your-azure-app-client-id>\"\n");
        return Ok(());
    }
    let raw = std::fs::read_to_string(&cfg_path).with_context(|| format!("read {cfg_path:?}"))?;
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
        // Try to trigger re-auth via the running daemon.
        match Connection::session().await {
            Ok(conn) => match make_proxy(&conn).await {
                Ok(proxy) => {
                    println!("Requesting re-authentication from daemon...");
                    match proxy.start_auth().await {
                        Ok((message, _user_code, _verification_uri)) => {
                            println!("{message}");
                            println!("\nWaiting for authentication to complete...");
                            println!("(The daemon will resume sync automatically when done.)");
                        }
                        Err(e) => {
                            eprintln!("Failed to start auth via daemon: {e}");
                            eprintln!("Is the daemon running? Try restarting it.");
                        }
                    }
                }
                Err(_) => {
                    eprintln!("Daemon not reachable via D-Bus.");
                    eprintln!("Restart the daemon to trigger re-authentication.");
                }
            },
            Err(e) => {
                eprintln!("D-Bus session not available: {e}");
                eprintln!("Restart the daemon to trigger re-authentication.");
            }
        }
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
        "SELECT id, local_path, name, size, is_placeholder FROM items WHERE pinned = 1 AND is_folder = 0 AND local_path LIKE ?1 ESCAPE '\\' ORDER BY local_path"
    } else {
        "SELECT id, local_path, name, size, is_placeholder FROM items WHERE pinned = 1 AND is_folder = 0 ORDER BY local_path"
    };

    let mut stmt = conn.prepare(query)?;
    let rows: Vec<(String, String, String, i64, bool)> = if let Some(ref p) = path_filter {
        let abs = absolutize(p);
        // Escape LIKE wildcards so a path containing % or _ can't over-match.
        let like = format!(
            "{}%",
            abs.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        stmt.query_map(rusqlite::params![like], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get::<_, i32>(4)? != 0,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get::<_, i32>(4)? != 0,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect()
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
    println!(
        "On device:    {on_device}/{total} ({:.0}%)",
        on_device as f64 / total as f64 * 100.0
    );
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

/// Best-effort absolute path: canonicalize, falling back to the raw input.
fn absolutize(path: &str) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::path::PathBuf::from(path))
        .to_string_lossy()
        .to_string()
}

fn config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/root/.config"))
        .join("onedrive-linux")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GB");
    }
}
