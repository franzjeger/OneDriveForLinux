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

impl Config {
    /// The default exclusion list, exposed so tests and callers can check
    /// behaviour against the same set the config ships with.
    pub fn default_excluded_patterns() -> Vec<String> {
        default_excluded()
    }
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

/// Files On-Demand downloads every file you open into the cache. Without a
/// ceiling the cache only ever grows, which defeats the point of the mode.
fn default_max_cache_size_gb() -> f64 {
    10.0
}

/// How long a file must go untouched before its edit is uploaded.
///
/// Uploading the instant a file closes made the mount race itself: an atomic
/// save uploads a temp file it is about to rename away, a scratch file races
/// its own delete, and an editor saving repeatedly starts an upload per save.
/// Waiting for quiet removes all of that — at the cost of the edit living only
/// on this machine until the wait is up, which is why the daemon flushes the
/// queue before it shuts down.
fn default_upload_debounce_secs() -> u64 {
    900
}

fn default_auth_method() -> String {
    "auto".into()
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

    /// Upper bound on the on-demand file cache, in GB. When the cache exceeds
    /// this, the least recently used files are evicted — never pinned files,
    /// and never files whose upload has not completed. `0` disables the limit.
    #[serde(default = "default_max_cache_size_gb")]
    pub max_cache_size_gb: f64,

    /// Seconds a file must go untouched before its edit is uploaded. `0`
    /// uploads as soon as the file is closed, which is what the client used to
    /// do — see [`default_upload_debounce_secs`] for why that was a problem.
    #[serde(default = "default_upload_debounce_secs")]
    pub upload_debounce_secs: u64,

    /// How to sign in: "auto" (browser when a desktop session is present),
    /// "browser" (authorization code + PKCE — required when Conditional
    /// Access blocks the device code flow), or "device_code".
    #[serde(default = "default_auth_method")]
    pub auth_method: String,
}

impl Config {
    /// `Some(true)` to force browser sign-in, `Some(false)` to force device
    /// code, `None` to let the auth layer decide from the environment.
    pub fn auth_preference(&self) -> Option<bool> {
        match self.auth_method.as_str() {
            "browser" => Some(true),
            "device_code" | "devicecode" => Some(false),
            _ => None,
        }
    }
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
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read config {path:?}"))?;
        let cfg: Config = toml::from_str(&raw).with_context(|| format!("parse config {path:?}"))?;
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
            max_cache_size_gb: default_max_cache_size_gb(),
            upload_debounce_secs: default_upload_debounce_secs(),
            auth_method: default_auth_method(),
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

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn minimal_config_gets_defaults() {
        let cfg: Config = toml::from_str(r#"client_id = "abc-123""#).unwrap();
        assert_eq!(cfg.client_id, "abc-123");
        assert_eq!(cfg.tenant_id, "common");
        assert!(cfg.on_demand);
        assert_eq!(cfg.max_upload_threads, 4);
        assert_eq!(cfg.max_download_threads, 4);
        assert_eq!(cfg.delta_poll_interval_secs, 30);
        assert_eq!(cfg.max_cache_size_gb, 10.0);
        assert!(cfg.excluded_patterns.contains(&"*.tmp".to_string()));
        assert!(cfg.sync_folders.is_empty());
    }

    #[test]
    fn auth_method_preference() {
        let auto: Config = toml::from_str(r#"client_id = "a""#).unwrap();
        assert_eq!(auto.auth_method, "auto");
        assert_eq!(auto.auth_preference(), None);

        let browser: Config =
            toml::from_str("client_id = \"a\"\nauth_method = \"browser\"").unwrap();
        assert_eq!(browser.auth_preference(), Some(true));

        let device: Config =
            toml::from_str("client_id = \"a\"\nauth_method = \"device_code\"").unwrap();
        assert_eq!(device.auth_preference(), Some(false));
    }

    #[test]
    fn missing_client_id_is_an_error() {
        assert!(toml::from_str::<Config>("on_demand = false").is_err());
    }

    #[test]
    fn explicit_values_override_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            client_id = "abc"
            tenant_id = "my-tenant"
            on_demand = false
            delta_poll_interval_secs = 5
            "#,
        )
        .unwrap();
        assert_eq!(cfg.tenant_id, "my-tenant");
        assert!(!cfg.on_demand);
        assert_eq!(cfg.delta_poll_interval_secs, 5);
    }
}
