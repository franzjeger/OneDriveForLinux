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

    #[error("Rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
}

pub type GraphResult<T> = Result<T, GraphError>;
