use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_sync_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/root"))
        .join("OneDrive")
}

fn default_tenant() -> String {
    "common".into()
}

fn default_on_demand() -> bool {
    true
}

fn default_threads() -> usize {
    4
}

fn default_poll_interval() -> u64 {
    30
}

fn default_excluded() -> Vec<String> {
    vec![
        "*.tmp".into(),
        "~$*".into(),
        ".~lock.*".into(),
        "desktop.ini".into(),
        "thumbs.db".into(),
    ]
}

fn default_sync_folders() -> Vec<String> {
    vec![]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Local directory to sync OneDrive files into.
    #[serde(default = "default_sync_dir")]
    pub sync_dir: PathBuf,

    /// Azure AD application client ID (user registers their own app).
    pub client_id: String,

    /// Tenant ID; use "common" for personal accounts.
    #[serde(default = "default_tenant")]
    pub tenant_id: String,

    /// Glob patterns to exclude from sync.
    #[serde(default = "default_excluded")]
    pub excluded_patterns: Vec<String>,

    /// If non-empty, only sync items whose OneDrive path starts with one of
    /// these top-level folder names (e.g. ["Projects", "Documents"]).
    /// Items outside these folders are recorded in the DB but never downloaded.
    /// Ignored when on_demand = true.
    #[serde(default = "default_sync_folders")]
    pub sync_folders: Vec<String>,

    /// Enable Files On-Demand via FUSE.
    #[serde(default = "default_on_demand")]
    pub on_demand: bool,

    /// Number of concurrent upload threads.
    #[serde(default = "default_threads")]
    pub max_upload_threads: usize,

    /// Number of concurrent download threads.
    #[serde(default = "default_threads")]
    pub max_download_threads: usize,

    /// Seconds between delta polls.
    #[serde(default = "default_poll_interval")]
    pub delta_poll_interval_secs: u64,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            anyhow::bail!(
                "Config file not found at {path:?}. \
                 Please create it with at least `client_id = \"<your-azure-client-id>\"`"
            );
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {path:?}"))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parse config {path:?}"))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }

    /// Write a starter config if none exists.
    pub fn write_default(client_id: &str) -> Result<()> {
        let cfg = Config {
            sync_dir: default_sync_dir(),
            client_id: client_id.to_string(),
            tenant_id: default_tenant(),
            excluded_patterns: default_excluded(),
            sync_folders: default_sync_folders(),
            on_demand: true,
            max_upload_threads: 4,
            max_download_threads: 4,
            delta_poll_interval_secs: 30,
        };
        cfg.save()
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/root/.config"))
        .join("onedrive-linux")
        .join("config.toml")
}
