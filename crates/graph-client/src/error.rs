use thiserror::Error;

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("OAuth2 error: {0}")]
    Auth(String),

    #[error("Token expired and refresh failed: {0}")]
    TokenRefresh(String),

    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },

    #[error("Item not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("Upload session error: {0}")]
    UploadSession(String),

    /// Graph invalidated our delta token (HTTP 410 `resyncRequired`). The
    /// stored deltaLink must be discarded and the sync restarted from scratch;
    /// retrying the same link loops forever.
    #[error("Delta token expired — a full resync is required")]
    ResyncRequired,

    /// The item changed on OneDrive since the version we based our edit on
    /// (HTTP 412). Overwriting anyway would destroy whoever else's change that
    /// was, so the caller has to reconcile rather than retry.
    #[error("The file changed on OneDrive since it was last synced")]
    Conflict,

    #[error("Rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
}

pub type GraphResult<T> = Result<T, GraphError>;
