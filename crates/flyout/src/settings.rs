//! Reading and writing `config.toml` on behalf of the settings view.
//!
//! Edits are applied key-by-key to the parsed document rather than by
//! serialising a whole struct, so keys this UI does not expose (client_id,
//! tenant_id, thread counts, anything added later) survive a save untouched.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// The subset of the config the settings view edits.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub sync_dir: String,
    pub on_demand: bool,
    pub poll_interval_secs: u64,
    pub auth_method: String,
    /// One glob per line, as shown in the text box.
    pub excluded_patterns: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sync_dir: default_sync_dir(),
            on_demand: true,
            poll_interval_secs: 30,
            auth_method: "auto".into(),
            excluded_patterns: ["*.tmp", "~$*", ".~lock.*", "desktop.ini", "thumbs.db"].join("\n"),
        }
    }
}

/// Sign-in methods offered in the UI, with the label shown for each.
pub const AUTH_METHODS: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("browser", "Browser sign-in"),
    ("device_code", "Device code"),
];

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/root/.config"))
        .join("onedrive-linux")
        .join("config.toml")
}

fn default_sync_dir() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/root"))
        .join("OneDrive")
        .to_string_lossy()
        .to_string()
}

/// Read the current settings, falling back to defaults for anything absent.
/// A missing config file is not an error — it yields defaults.
pub fn load() -> Result<Settings> {
    let path = config_path();
    if !path.exists() {
        return Ok(Settings::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {path:?}"))?;
    let doc: toml::Table = toml::from_str(&raw).with_context(|| format!("parse {path:?}"))?;
    let defaults = Settings::default();

    Ok(Settings {
        sync_dir: doc
            .get("sync_dir")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or(defaults.sync_dir),
        on_demand: doc
            .get("on_demand")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.on_demand),
        poll_interval_secs: doc
            .get("delta_poll_interval_secs")
            .and_then(|v| v.as_integer())
            .map(|v| v.max(1) as u64)
            .unwrap_or(defaults.poll_interval_secs),
        auth_method: doc
            .get("auth_method")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or(defaults.auth_method),
        excluded_patterns: doc
            .get("excluded_patterns")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or(defaults.excluded_patterns),
    })
}

/// Apply `settings` to the config file, leaving every other key as it was.
pub fn save(settings: &Settings) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {parent:?}"))?;
    }

    // Start from the existing document so unmanaged keys are carried over.
    let mut doc: toml::Table = if path.exists() {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("read {path:?}"))?;
        toml::from_str(&raw).with_context(|| format!("parse {path:?}"))?
    } else {
        toml::Table::new()
    };

    doc.insert("sync_dir".into(), settings.sync_dir.trim().into());
    doc.insert("on_demand".into(), settings.on_demand.into());
    doc.insert(
        "delta_poll_interval_secs".into(),
        (settings.poll_interval_secs.max(1) as i64).into(),
    );
    doc.insert("auth_method".into(), settings.auth_method.clone().into());
    doc.insert(
        "excluded_patterns".into(),
        toml::Value::Array(
            settings
                .excluded_patterns
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| toml::Value::String(line.to_string()))
                .collect(),
        ),
    );

    let rendered = toml::to_string_pretty(&doc).context("serialise config")?;

    // Write via a temp file and rename so a crash mid-write cannot leave a
    // truncated config that the daemon then refuses to start with.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, rendered).with_context(|| format!("write {tmp:?}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {path:?}"))?;
    Ok(())
}

/// Restart the daemon so the new configuration takes effect. The config is
/// read once at startup, so saving alone changes nothing.
pub fn restart_daemon() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "restart", "onedrive-linux.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(initial: &str, edit: impl FnOnce(&mut Settings)) -> toml::Table {
        // save() targets the real config path, so exercise its logic directly
        // on an in-memory document instead.
        let mut doc: toml::Table = toml::from_str(initial).unwrap();
        let mut settings = Settings {
            sync_dir: doc
                .get("sync_dir")
                .and_then(|v| v.as_str())
                .unwrap_or("/home/u/OneDrive")
                .into(),
            on_demand: doc
                .get("on_demand")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            poll_interval_secs: doc
                .get("delta_poll_interval_secs")
                .and_then(|v| v.as_integer())
                .unwrap_or(30) as u64,
            auth_method: doc
                .get("auth_method")
                .and_then(|v| v.as_str())
                .unwrap_or("auto")
                .into(),
            excluded_patterns: String::new(),
        };
        edit(&mut settings);
        doc.insert("sync_dir".into(), settings.sync_dir.trim().into());
        doc.insert("on_demand".into(), settings.on_demand.into());
        doc.insert(
            "delta_poll_interval_secs".into(),
            (settings.poll_interval_secs.max(1) as i64).into(),
        );
        doc.insert("auth_method".into(), settings.auth_method.clone().into());
        doc
    }

    #[test]
    fn saving_preserves_keys_the_ui_does_not_manage() {
        let doc = round_trip(
            "client_id = \"abc-123\"\ntenant_id = \"my-tenant\"\nmax_upload_threads = 8\n",
            |s| s.on_demand = false,
        );
        assert_eq!(doc["client_id"].as_str(), Some("abc-123"));
        assert_eq!(doc["tenant_id"].as_str(), Some("my-tenant"));
        assert_eq!(doc["max_upload_threads"].as_integer(), Some(8));
        assert_eq!(doc["on_demand"].as_bool(), Some(false));
    }

    #[test]
    fn poll_interval_never_saves_as_zero() {
        let doc = round_trip("client_id = \"a\"\n", |s| s.poll_interval_secs = 0);
        assert_eq!(doc["delta_poll_interval_secs"].as_integer(), Some(1));
    }

    #[test]
    fn defaults_are_a_valid_starting_point() {
        let s = Settings::default();
        assert!(s.on_demand);
        assert_eq!(s.auth_method, "auto");
        assert!(s.excluded_patterns.contains("*.tmp"));
        assert!(AUTH_METHODS.iter().any(|(id, _)| *id == s.auth_method));
    }

    #[test]
    fn patterns_round_trip_through_the_text_box_format() {
        let s = Settings {
            excluded_patterns: "*.tmp\n\n  ~$*  \n".into(),
            ..Settings::default()
        };
        let parsed: Vec<&str> = s
            .excluded_patterns
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(parsed, vec!["*.tmp", "~$*"]);
    }
}
