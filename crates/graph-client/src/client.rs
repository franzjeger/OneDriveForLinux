use crate::{
    auth::AuthManager,
    error::{GraphError, GraphResult},
    models::{
        CreateFolderRequest, CreateUploadSessionRequest, DeltaResponse, DriveItem, MoveItemRequest,
        MoveParentReference, UploadSession, UploadSessionItem,
    },
};
use bytes::Bytes;
use futures::StreamExt;
use std::{path::Path, sync::Arc};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Characters that must be percent-encoded when a file name is embedded in a
/// URL path segment. Without this, names containing `#`, `?`, `%`, etc. are
/// silently misinterpreted by the Graph API.
const PATH_SEGMENT_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%')
    .add(b'/')
    .add(b'\\');

fn encode_name(name: &str) -> String {
    percent_encoding::utf8_percent_encode(name, PATH_SEGMENT_ENCODE).to_string()
}
/// Files larger than this use an upload session.
const LARGE_FILE_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MB
/// Chunk size for upload sessions (must be multiple of 320 KiB per Graph API).
const UPLOAD_CHUNK_SIZE: usize = 10 * 320 * 1024; // ~3.2 MB
/// Maximum number of retries for transient failures.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (doubles each retry).
const RETRY_BASE_DELAY_SECS: u64 = 2;

pub struct GraphClient {
    http: reqwest::Client,
    auth: Arc<AuthManager>,
}

impl GraphClient {
    pub fn new(auth: Arc<AuthManager>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("OneDriveForLinux/0.1")
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("build reqwest client");
        Self { http, auth }
    }

    // ── Auth helper ────────────────────────────────────────────────────────────

    async fn bearer(&self) -> GraphResult<String> {
        self.auth
            .get_access_token()
            .await
            .map_err(|e| GraphError::Auth(e.to_string()))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> GraphResult<T> {
        let resp = self
            .request_with_retry(|| async {
                let token = self.bearer().await?;
                let resp = self.http.get(url).bearer_auth(&token).send().await?;
                Ok(resp)
            })
            .await?;
        let val: T = resp.json().await?;
        Ok(val)
    }

    /// Execute a request closure with automatic retry on transient errors.
    /// Retries on: 429 (rate limit), 500, 502, 503, 504 (server errors),
    /// and network/timeout errors. Uses exponential backoff, respecting
    /// Retry-After headers from 429 responses.
    async fn request_with_retry<F, Fut>(&self, make_request: F) -> GraphResult<reqwest::Response>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = GraphResult<reqwest::Response>>,
    {
        let mut attempt = 0u32;
        loop {
            let result = make_request().await;
            attempt += 1;

            match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    match status {
                        200..=299 => return Ok(resp),
                        401 => {
                            if attempt > 1 {
                                // Already retried once after a refresh — give up.
                                return Err(GraphError::Auth(
                                    "401 Unauthorized after token refresh — re-authentication required".into(),
                                ));
                            }
                            warn!("Got 401, forcing token refresh (attempt {attempt})");
                            if let Err(e) = self.auth.force_refresh().await {
                                return Err(GraphError::Auth(format!(
                                    "Token refresh failed after 401: {e} — re-authentication required"
                                )));
                            }
                            // Retry with the new token.
                        }
                        429 => {
                            let retry_secs = resp
                                .headers()
                                .get("Retry-After")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(30);
                            if attempt > MAX_RETRIES {
                                return Err(GraphError::RateLimited {
                                    retry_after_secs: retry_secs,
                                });
                            }
                            warn!("Rate limited (429), retry {attempt}/{MAX_RETRIES} after {retry_secs}s");
                            tokio::time::sleep(std::time::Duration::from_secs(retry_secs)).await;
                        }
                        404 => return Err(GraphError::NotFound(status.to_string())),
                        500 | 502 | 503 | 504 => {
                            if attempt > MAX_RETRIES {
                                return Err(GraphError::Api {
                                    status,
                                    message: resp
                                        .status()
                                        .canonical_reason()
                                        .unwrap_or("server error")
                                        .into(),
                                });
                            }
                            let delay = RETRY_BASE_DELAY_SECS * 2u64.pow(attempt - 1);
                            warn!("Server error ({status}), retry {attempt}/{MAX_RETRIES} after {delay}s");
                            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        }
                        _ => {
                            // Graph returns a JSON error body with the actual
                            // failure reason — include it for diagnosability.
                            let reason = resp.status().canonical_reason().unwrap_or("unknown");
                            let body = resp.text().await.unwrap_or_default();
                            let body: String = body.chars().take(512).collect();
                            return Err(GraphError::Api {
                                status,
                                message: if body.is_empty() {
                                    reason.into()
                                } else {
                                    format!("{reason}: {body}")
                                },
                            });
                        }
                    }
                }
                Err(GraphError::Http(e)) if e.is_timeout() || e.is_connect() => {
                    if attempt > MAX_RETRIES {
                        return Err(GraphError::Http(e));
                    }
                    let delay = RETRY_BASE_DELAY_SECS * 2u64.pow(attempt - 1);
                    warn!("Network error ({e}), retry {attempt}/{MAX_RETRIES} after {delay}s");
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ── Drive root ─────────────────────────────────────────────────────────────

    pub async fn get_drive_root(&self) -> GraphResult<DriveItem> {
        let url = format!("{GRAPH_BASE}/me/drive/root");
        debug!("GET {url}");
        self.get_json(&url).await
    }

    // ── Children ───────────────────────────────────────────────────────────────

    pub async fn list_children(&self, item_id: &str) -> GraphResult<Vec<DriveItem>> {
        let mut items = Vec::new();
        let mut url = format!(
            "{GRAPH_BASE}/me/drive/items/{item_id}/children\
             ?$select=id,name,eTag,cTag,size,createdDateTime,lastModifiedDateTime,\
             file,folder,parentReference,fileSystemInfo,deleted"
        );

        loop {
            let page: DeltaResponse = self.get_json(&url).await?;
            items.extend(page.items);
            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }

        debug!("list_children({item_id}): {} items", items.len());
        Ok(items)
    }

    // ── Delta ──────────────────────────────────────────────────────────────────

    pub async fn get_delta(
        &self,
        folder_id: &str,
        delta_link: Option<&str>,
    ) -> GraphResult<DeltaResponse> {
        let url = match delta_link {
            Some(link) => link.to_string(),
            // No $select — omitting it ensures ALL facets (folder, file, deleted,
            // remoteItem, etc.) are returned reliably. With $select the Graph API
            // sometimes silently drops facets like `folder` for delta responses,
            // causing folders to be misidentified as files.
            None => format!("{GRAPH_BASE}/me/drive/items/{folder_id}/delta"),
        };

        debug!("delta url: {url}");

        // Collect all pages into one response so callers get a complete batch.
        let mut all_items = Vec::new();
        let mut current_url = url;

        let final_delta_link = loop {
            let resp = self
                .request_with_retry(|| async {
                    let token = self.bearer().await?;
                    let resp = self
                        .http
                        .get(&current_url)
                        .bearer_auth(&token)
                        .send()
                        .await?;
                    Ok(resp)
                })
                .await?;

            // Parse via serde_json::Value first so that individual items with
            // unexpected field types don't abort the whole page.
            let raw: serde_json::Value = resp.json().await?;
            let page_items = raw["value"].as_array().cloned().unwrap_or_default();
            for item_val in page_items {
                match serde_json::from_value::<DriveItem>(item_val) {
                    Ok(item) => all_items.push(item),
                    Err(e) => warn!("Skipping unparseable delta item: {e}"),
                }
            }
            let next_link = raw["@odata.nextLink"].as_str().map(String::from);
            let delta_link = raw["@odata.deltaLink"].as_str().map(String::from);

            if let Some(next) = next_link {
                current_url = next;
            } else {
                break delta_link;
            }
        };

        Ok(DeltaResponse {
            items: all_items,
            next_link: None,
            delta_link: final_delta_link,
        })
    }

    // ── Download ───────────────────────────────────────────────────────────────

    pub async fn download_file(&self, item_id: &str, dest: &Path) -> GraphResult<()> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}/content");
        debug!("download_file({item_id}) -> {dest:?}");

        let resp = self
            .request_with_retry(|| async {
                let token = self.bearer().await?;
                let resp = self.http.get(&url).bearer_auth(&token).send().await?;
                Ok(resp)
            })
            .await?;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Atomic download: write to a temp file first, then rename.
        // If the daemon crashes mid-download, only the .tmp file is left
        // (and will be cleaned up on next run), not a corrupt target file.
        let tmp_path = dest.with_extension(format!(
            "{}.tmp",
            dest.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));

        let result = async {
            let mut file = tokio::fs::File::create(&tmp_path).await?;
            let mut stream = resp.bytes_stream();

            while let Some(chunk) = stream.next().await {
                let chunk: Bytes = chunk?;
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            file.sync_all().await?;
            GraphResult::Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                tokio::fs::rename(&tmp_path, dest).await?;
                info!("Downloaded item {item_id} to {dest:?}");
                Ok(())
            }
            Err(e) => {
                // Clean up partial temp file on failure.
                let _ = tokio::fs::remove_file(&tmp_path).await;
                Err(e)
            }
        }
    }

    // ── Upload ─────────────────────────────────────────────────────────────────

    pub async fn upload_file(
        &self,
        parent_id: &str,
        name: &str,
        path: &Path,
    ) -> GraphResult<DriveItem> {
        let metadata = tokio::fs::metadata(path).await?;
        let size = metadata.len();

        if size <= LARGE_FILE_THRESHOLD {
            self.upload_small(parent_id, name, path).await
        } else {
            let session = self.get_upload_session(parent_id, name, size).await?;
            self.upload_via_session(&session, path, size).await
        }
    }

    async fn upload_small(
        &self,
        parent_id: &str,
        name: &str,
        path: &Path,
    ) -> GraphResult<DriveItem> {
        let url = format!(
            "{GRAPH_BASE}/me/drive/items/{parent_id}:/{}:/content",
            encode_name(name)
        );
        debug!("upload_small -> {url}");

        let data = tokio::fs::read(path).await?;
        let content_length = data.len();

        let resp = self
            .request_with_retry(|| {
                let data = data.clone();
                let url = url.clone();
                async move {
                    let token = self.bearer().await?;
                    let resp = self
                        .http
                        .put(&url)
                        .bearer_auth(&token)
                        .header("Content-Type", "application/octet-stream")
                        .header("Content-Length", content_length.to_string())
                        .body(data)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;
        let item: DriveItem = resp.json().await?;
        info!("Uploaded (small) {name} -> id={}", item.id);
        Ok(item)
    }

    pub async fn get_upload_session(
        &self,
        parent_id: &str,
        name: &str,
        _size: u64,
    ) -> GraphResult<UploadSession> {
        let url = format!(
            "{GRAPH_BASE}/me/drive/items/{parent_id}:/{}:/createUploadSession",
            encode_name(name)
        );
        let body = CreateUploadSessionRequest {
            item: UploadSessionItem {
                conflict_behavior: "replace".into(),
                name: name.to_string(),
            },
        };

        let resp = self
            .request_with_retry(|| async {
                let token = self.bearer().await?;
                let resp = self
                    .http
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await?;
                Ok(resp)
            })
            .await?;
        let session: UploadSession = resp.json().await?;
        debug!("Upload session created: {}", session.upload_url);
        Ok(session)
    }

    async fn upload_via_session(
        &self,
        session: &UploadSession,
        path: &Path,
        total_size: u64,
    ) -> GraphResult<DriveItem> {
        use tokio::io::AsyncReadExt;

        // Stream the file chunk-by-chunk — this path handles files above the
        // small-upload threshold, which can be arbitrarily large, so the whole
        // file must never be held in memory at once.
        let mut file = tokio::fs::File::open(path).await?;
        let mut offset: u64 = 0;
        let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];

        while offset < total_size {
            let chunk_len = ((total_size - offset) as usize).min(UPLOAD_CHUNK_SIZE);
            file.read_exact(&mut buf[..chunk_len]).await?;
            // Bytes clones are refcounted — retries resend without recopying.
            let chunk = Bytes::copy_from_slice(&buf[..chunk_len]);
            let end = offset + chunk_len as u64;
            let range = format!("bytes {}-{}/{}", offset, end - 1, total_size);
            debug!("Uploading chunk: {range}");

            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let result = self
                    .http
                    .put(&session.upload_url)
                    .header("Content-Range", &range)
                    .header("Content-Length", chunk_len.to_string())
                    .body(chunk.clone())
                    .send()
                    .await;

                let resp = match result {
                    Ok(resp) => resp,
                    Err(e) if (e.is_timeout() || e.is_connect()) && attempt <= MAX_RETRIES => {
                        let delay = RETRY_BASE_DELAY_SECS * 2u64.pow(attempt - 1);
                        warn!("Network error uploading {range} ({e}), retry {attempt}/{MAX_RETRIES} after {delay}s");
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        continue;
                    }
                    Err(e) => return Err(GraphError::Http(e)),
                };

                let status = resp.status().as_u16();
                match status {
                    200 | 201 => {
                        // Upload complete — Graph returns the DriveItem
                        let item: DriveItem = resp.json().await?;
                        info!("Upload complete via session: id={}", item.id);
                        return Ok(item);
                    }
                    202 => {
                        // Chunk accepted, continue with the next one.
                        if end >= total_size {
                            warn!("Uploaded all bytes but got 202 — unexpected");
                            return Err(GraphError::UploadSession(
                                "unexpected 202 after final chunk".into(),
                            ));
                        }
                        break;
                    }
                    429 if attempt <= MAX_RETRIES => {
                        let retry = resp
                            .headers()
                            .get("Retry-After")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(30);
                        warn!("Rate limited during upload, retry {attempt}/{MAX_RETRIES} after {retry}s");
                        tokio::time::sleep(std::time::Duration::from_secs(retry)).await;
                    }
                    500..=504 if attempt <= MAX_RETRIES => {
                        let delay = RETRY_BASE_DELAY_SECS * 2u64.pow(attempt - 1);
                        warn!("Server error ({status}) uploading {range}, retry {attempt}/{MAX_RETRIES} after {delay}s");
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    }
                    _ => {
                        let body = resp.text().await.unwrap_or_default();
                        return Err(GraphError::UploadSession(format!(
                            "Unexpected status {status} at range {range}: {}",
                            body.chars().take(512).collect::<String>()
                        )));
                    }
                }
            }
            offset = end;
        }

        Err(GraphError::UploadSession(
            "upload session ended without completion response".into(),
        ))
    }

    // ── Folder / item operations ───────────────────────────────────────────────

    pub async fn create_folder(&self, parent_id: &str, name: &str) -> GraphResult<DriveItem> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{parent_id}/children");
        let body = CreateFolderRequest {
            name: name.to_string(),
            folder: serde_json::json!({}),
            conflict_behavior: "rename".into(),
        };

        let resp = self
            .request_with_retry(|| async {
                let token = self.bearer().await?;
                let resp = self
                    .http
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await?;
                Ok(resp)
            })
            .await?;
        let item: DriveItem = resp.json().await?;
        info!("Created folder '{name}' id={}", item.id);
        Ok(item)
    }

    pub async fn delete_item(&self, item_id: &str) -> GraphResult<()> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}");
        let resp = self
            .request_with_retry(|| async {
                let token = self.bearer().await?;
                let resp = self.http.delete(&url).bearer_auth(&token).send().await?;
                Ok(resp)
            })
            .await?;
        // 204 = success for delete
        if resp.status().as_u16() == 204 || resp.status().is_success() {
            info!("Deleted item {item_id}");
            return Ok(());
        }
        // request_with_retry already checked for errors, but handle edge cases
        Err(GraphError::Api {
            status: resp.status().as_u16(),
            message: resp.status().canonical_reason().unwrap_or("unknown").into(),
        })
    }

    pub async fn move_item(
        &self,
        item_id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> GraphResult<DriveItem> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}");
        let body = MoveItemRequest {
            parent_reference: MoveParentReference {
                id: new_parent_id.to_string(),
            },
            name: new_name.to_string(),
        };

        let resp = self
            .request_with_retry(|| async {
                let token = self.bearer().await?;
                let resp = self
                    .http
                    .patch(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await?;
                Ok(resp)
            })
            .await?;
        let item: DriveItem = resp.json().await?;
        info!("Moved item {item_id} -> parent={new_parent_id} name={new_name}");
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::encode_name;

    #[test]
    fn plain_names_unchanged() {
        assert_eq!(encode_name("report.docx"), "report.docx");
    }

    #[test]
    fn special_characters_are_escaped() {
        assert_eq!(encode_name("a b#c?.txt"), "a%20b%23c%3F.txt");
        assert_eq!(encode_name("50%.txt"), "50%25.txt");
        assert_eq!(encode_name("a/b"), "a%2Fb");
    }

    #[test]
    fn unicode_is_preserved_via_utf8_encoding() {
        assert_eq!(encode_name("møte.txt"), "m%C3%B8te.txt");
    }
}
