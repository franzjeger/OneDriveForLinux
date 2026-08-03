use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Per-item sync state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncState {
    /// Local and remote are in agreement.
    Synced,
    /// Item is currently being transferred.
    Syncing,
    /// A sync error occurred.
    Error(String),
    /// Item exists locally only (not yet uploaded).
    LocalOnly,
    /// Item exists in cloud only (placeholder or not yet downloaded).
    CloudOnly,
    /// Local and remote versions differ — conflict must be resolved.
    Conflict,
    /// Item is pinned — always kept on device, never evicted to cloud-only.
    Pinned,
    /// Folder has a mix of local and cloud-only descendants (computed, never stored in DB).
    Partial,
}

impl SyncState {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            SyncState::Synced => "synced",
            SyncState::Syncing => "syncing",
            SyncState::Error(_) => "error",
            SyncState::LocalOnly => "local_only",
            SyncState::CloudOnly => "cloud_only",
            SyncState::Conflict => "conflict",
            SyncState::Pinned => "pinned",
            SyncState::Partial => "partial",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "synced" => SyncState::Synced,
            "syncing" => SyncState::Syncing,
            "local_only" => SyncState::LocalOnly,
            "cloud_only" => SyncState::CloudOnly,
            "conflict" => SyncState::Conflict,
            "pinned" => SyncState::Pinned,
            other => SyncState::Error(other.to_string()),
        }
    }
}

impl std::fmt::Display for SyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncState::Synced => write!(f, "Synced"),
            SyncState::Syncing => write!(f, "Syncing"),
            SyncState::Error(e) => write!(f, "Error: {e}"),
            SyncState::LocalOnly => write!(f, "Local only"),
            SyncState::CloudOnly => write!(f, "Cloud only"),
            SyncState::Conflict => write!(f, "Conflict"),
            SyncState::Pinned => write!(f, "Pinned"),
            SyncState::Partial => write!(f, "Partially synced"),
        }
    }
}

/// Events broadcast over the sync event channel.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// An item's sync state changed.
    ItemStateChanged { path: PathBuf, state: SyncState },
    /// Overall sync pass started.
    SyncStarted,
    /// Human-readable progress during a long pass ("Fetching file list… 1200 items").
    SyncProgress(String),
    /// Overall sync pass completed.
    SyncCompleted,
    /// Sync is paused.
    Paused,
    /// Sync is resumed.
    Resumed,
    /// A non-fatal error occurred.
    Error(String),
    /// Authentication required.
    AuthRequired,
    /// An upload was retried to exhaustion and has been given up on. Carries
    /// enough to tell the user which file, since this may cost them an edit.
    UploadFailed { name: String, error: String },
    /// The network is unreachable — distinct from a genuine sync error, which
    /// needs a different message and a different icon.
    Offline,
    /// Connectivity is back.
    BackOnline,
}

#[cfg(test)]
mod tests {
    use super::SyncState;

    #[test]
    fn db_str_roundtrip() {
        for state in [
            SyncState::Synced,
            SyncState::Syncing,
            SyncState::LocalOnly,
            SyncState::CloudOnly,
            SyncState::Conflict,
            SyncState::Pinned,
        ] {
            assert_eq!(SyncState::from_db_str(state.as_db_str()), state);
        }
    }

    #[test]
    fn unknown_db_str_becomes_error() {
        assert_eq!(
            SyncState::from_db_str("bogus"),
            SyncState::Error("bogus".to_string())
        );
    }
}
