use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token";
/// OAuth scopes requested for both the device-code and refresh flows.
const OAUTH_SCOPE: &str = "Files.ReadWrite.All offline_access User.Read";

const DEVICE_CODE_ENDPOINT: &str =
    "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode";

/// Persisted token set saved to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub token_type: String,
    pub scope: String,
}

impl TokenSet {
    pub fn is_expired(&self) -> bool {
        Utc::now() + Duration::seconds(60) >= self.expires_at
    }
}

/// Raw response from the token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    token_type: String,
    scope: String,
}

/// Response from the device code endpoint.
#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct DeviceCodePollResponse {
    #[serde(default)]
    error: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    token_type: Option<String>,
    scope: Option<String>,
}

pub struct AuthManager {
    client_id: String,
    tenant_id: String,
    http: reqwest::Client,
    token: RwLock<Option<TokenSet>>,
    token_path: PathBuf,
    /// Serializes token refreshes. Microsoft rotates refresh tokens, so two
    /// concurrent refreshes with the same token can invalidate the grant and
    /// force a full re-authentication.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl AuthManager {
    pub fn new(client_id: String, tenant_id: String) -> Result<Self> {
        let config_dir = dirs_path();
        std::fs::create_dir_all(&config_dir).context("create config dir")?;
        let token_path = config_dir.join("tokens.json");

        let http = reqwest::Client::builder()
            .user_agent("OneDriveForLinux/0.1")
            .build()?;

        let token = if token_path.exists() {
            let data = std::fs::read_to_string(&token_path).context("read tokens.json")?;
            let ts: TokenSet = serde_json::from_str(&data).context("parse tokens.json")?;
            debug!("Loaded existing token set from disk");
            Some(ts)
        } else {
            None
        };

        Ok(Self {
            client_id,
            tenant_id,
            http,
            token: RwLock::new(token),
            token_path,
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Test-only constructor with a pre-seeded token and isolated token path,
    /// so tests never touch the real config directory or the network.
    #[cfg(test)]
    pub(crate) fn for_tests(token: TokenSet, token_path: PathBuf) -> Self {
        Self {
            client_id: "test-client".into(),
            tenant_id: "common".into(),
            http: reqwest::Client::new(),
            token: RwLock::new(Some(token)),
            token_path,
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Returns true if we already have a valid (or refreshable) token.
    pub async fn is_authenticated(&self) -> bool {
        self.token.read().await.is_some()
    }

    /// Performs the OAuth2 device code flow interactively.
    /// Prints user code and verification URI to stdout.
    pub async fn authenticate_device_code(&self) -> Result<()> {
        let dc = self.request_device_code().await?;

        println!("\n{}", dc.message);
        println!(
            "Open: {}\nEnter code: {}",
            dc.verification_uri, dc.user_code
        );

        let ts = self.poll_for_token(&dc).await?;
        self.save_token(ts).await?;
        info!("Authentication successful");
        Ok(())
    }

    /// Start the device code flow and return the response (user_code, verification_uri, etc.)
    /// for display to the user. Call `complete_device_auth(dc)` to poll for the token.
    pub async fn start_device_code_flow(&self) -> Result<DeviceCodeResponse> {
        self.request_device_code().await
    }

    /// Poll for token after device code flow was initiated with `start_device_code_flow()`.
    /// Saves the token when received. Returns Ok(()) when authenticated.
    pub async fn complete_device_auth(&self, dc: DeviceCodeResponse) -> Result<()> {
        let ts = self.poll_for_token(&dc).await?;
        self.save_token(ts).await?;
        info!("Re-authentication successful");
        Ok(())
    }

    async fn request_device_code(&self) -> Result<DeviceCodeResponse> {
        let url = DEVICE_CODE_ENDPOINT.replace("{tenant}", &self.tenant_id);
        let params = [
            ("client_id", self.client_id.as_str()),
            ("scope", OAUTH_SCOPE),
        ];
        let resp = self.http.post(&url).form(&params).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Device code request failed ({status}): {body}");
        }
        let dc: DeviceCodeResponse = resp.json().await?;
        Ok(dc)
    }

    async fn poll_for_token(&self, dc: &DeviceCodeResponse) -> Result<TokenSet> {
        let url = TOKEN_ENDPOINT.replace("{tenant}", &self.tenant_id);
        let interval = std::time::Duration::from_secs(dc.interval.max(5));
        // The device code itself expires — stop polling once it does instead
        // of looping forever on an abandoned sign-in.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(dc.expires_in);

        loop {
            tokio::time::sleep(interval).await;
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "Device code expired after {}s without sign-in — please try again",
                    dc.expires_in
                );
            }

            let params = [
                ("client_id", self.client_id.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", dc.device_code.as_str()),
            ];

            let resp: DeviceCodePollResponse = self
                .http
                .post(&url)
                .form(&params)
                .send()
                .await?
                .json()
                .await?;

            match resp.error.as_deref() {
                None => {
                    // Success
                    let ts = TokenSet {
                        access_token: resp.access_token.context("missing access_token")?,
                        refresh_token: resp.refresh_token,
                        expires_at: Utc::now() + Duration::seconds(resp.expires_in.unwrap_or(3600)),
                        token_type: resp.token_type.unwrap_or_else(|| "Bearer".into()),
                        scope: resp.scope.unwrap_or_default(),
                    };
                    return Ok(ts);
                }
                Some("authorization_pending") => {
                    debug!("Device code: authorization pending");
                    continue;
                }
                Some("slow_down") => {
                    warn!("Device code: slow_down requested");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                Some(e) => {
                    anyhow::bail!("Device code flow failed: {e}");
                }
            }
        }
    }

    /// Returns a valid access token, refreshing if necessary.
    pub async fn get_access_token(&self) -> Result<String> {
        // Fast path: token is still valid
        {
            let guard = self.token.read().await;
            if let Some(ts) = guard.as_ref() {
                if !ts.is_expired() {
                    return Ok(ts.access_token.clone());
                }
            }
        }

        // Slow path: refresh
        self.refresh_token_inner().await
    }

    /// Force a token refresh regardless of expiry, e.g. after a 401 response.
    /// Returns the new access token, or an error if the refresh token is missing
    /// or has been revoked (user must re-authenticate).
    pub async fn force_refresh(&self) -> Result<String> {
        self.refresh_token_inner().await
    }

    async fn refresh_token_inner(&self) -> Result<String> {
        // Snapshot the access token that prompted this refresh BEFORE queueing
        // on the lock, so we can tell whether another task already refreshed.
        let stale_token = {
            let guard = self.token.read().await;
            guard.as_ref().map(|ts| ts.access_token.clone())
        };

        let _guard = self.refresh_lock.lock().await;

        // Another task may have refreshed while we waited for the lock. If the
        // token changed, use it instead of spending our (single-use) refresh
        // token again. Works for both the expiry path and the 401 force path:
        // a 401'd token is by definition the stale one.
        {
            let guard = self.token.read().await;
            if let Some(ts) = guard.as_ref() {
                if Some(&ts.access_token) != stale_token.as_ref() {
                    return Ok(ts.access_token.clone());
                }
            }
        }

        let refresh_token = {
            let guard = self.token.read().await;
            guard
                .as_ref()
                .and_then(|ts| ts.refresh_token.clone())
                .context("No refresh token available — please re-authenticate")?
        };

        let url = TOKEN_ENDPOINT.replace("{tenant}", &self.tenant_id);
        let params = [
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("scope", OAUTH_SCOPE),
        ];

        let resp = self.http.post(&url).form(&params).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Token refresh failed ({status}): {body}");
        }

        let tr: TokenResponse = resp.json().await?;
        let ts = TokenSet {
            access_token: tr.access_token.clone(),
            refresh_token: tr.refresh_token,
            expires_at: Utc::now() + Duration::seconds(tr.expires_in),
            token_type: tr.token_type,
            scope: tr.scope,
        };

        self.save_token(ts).await?;
        info!("Token refreshed successfully");
        Ok(tr.access_token)
    }

    async fn save_token(&self, ts: TokenSet) -> Result<()> {
        let json = serde_json::to_string_pretty(&ts)?;
        tokio::fs::write(&self.token_path, json).await?;
        // Tokens grant full drive access — keep them readable by the owner only.
        let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
        tokio::fs::set_permissions(&self.token_path, perms).await?;
        *self.token.write().await = Some(ts);
        debug!("Token saved to {:?}", self.token_path);
        Ok(())
    }

    /// Clears all saved tokens (sign out).
    pub async fn sign_out(&self) -> Result<()> {
        *self.token.write().await = None;
        if self.token_path.exists() {
            tokio::fs::remove_file(&self.token_path).await?;
        }
        info!("Signed out — tokens removed");
        Ok(())
    }
}

fn dirs_path() -> PathBuf {
    // Must match the path odctl uses for sign-out (dirs::config_dir honors
    // XDG_CONFIG_HOME; a hand-built $HOME/.config path does not).
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/root/.config"))
        .join("onedrive-linux")
}
