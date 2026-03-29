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
/// Files larger than this use an upload session.
const LARGE_FILE_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MB
/// Chunk size for upload sessions (must be multiple of 320 KiB per Graph API).
const UPLOAD_CHUNK_SIZE: usize = 10 * 320 * 1024; // ~3.2 MB

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
        let token = self.bearer().await?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(&token)
            .send()
            .await?;

        self.check_status(&resp).await?;
        let val: T = resp.json().await?;
        Ok(val)
    }

    async fn check_status(&self, resp: &reqwest::Response) -> GraphResult<()> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status.as_u16() == 429 {
            let retry = resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30);
            return Err(GraphError::RateLimited {
                retry_after_secs: retry,
            });
        }
        if status.as_u16() == 404 {
            return Err(GraphError::NotFound(status.to_string()));
        }
        Err(GraphError::Api {
            status: status.as_u16(),
            message: status.canonical_reason().unwrap_or("unknown").into(),
        })
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
        let mut final_delta_link = None;

        loop {
            let token = self.bearer().await?;
            let resp = self
                .http
                .get(&current_url)
                .bearer_auth(&token)
                .send()
                .await?;
            self.check_status(&resp).await?;

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
                final_delta_link = delta_link;
                break;
            }
        }

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

        let resp = loop {
            let token = self.bearer().await?;
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&token)
                .send()
                .await?;
            if resp.status().as_u16() == 429 {
                let retry = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(30);
                warn!("Rate limited downloading {item_id}, sleeping {retry}s");
                tokio::time::sleep(std::time::Duration::from_secs(retry)).await;
                continue;
            }
            self.check_status(&resp).await?;
            break resp;
        };

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk: Bytes = chunk?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        info!("Downloaded item {item_id} to {dest:?}");
        Ok(())
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
        let url = format!("{GRAPH_BASE}/me/drive/items/{parent_id}:/{name}:/content");
        debug!("upload_small -> {url}");

        let data = tokio::fs::read(path).await?;
        let token = self.bearer().await?;
        let content_length = data.len();

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", content_length.to_string())
            .body(data)
            .send()
            .await?;
        self.check_status(&resp).await?;
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
            "{GRAPH_BASE}/me/drive/items/{parent_id}:/{name}:/createUploadSession"
        );
        let body = CreateUploadSessionRequest {
            item: UploadSessionItem {
                conflict_behavior: "replace".into(),
                name: name.to_string(),
            },
        };

        let token = self.bearer().await?;
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        self.check_status(&resp).await?;
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
        let data = tokio::fs::read(path).await?;
        let mut offset = 0usize;

        loop {
            let end = (offset + UPLOAD_CHUNK_SIZE).min(data.len());
            let chunk = &data[offset..end];
            let range = format!("bytes {}-{}/{}", offset, end - 1, total_size);
            debug!("Uploading chunk: {range}");

            let resp = self
                .http
                .put(&session.upload_url)
                .header("Content-Range", &range)
                .header("Content-Length", chunk.len().to_string())
                .body(chunk.to_vec())
                .send()
                .await?;

            let status = resp.status().as_u16();
            match status {
                200 | 201 => {
                    // Upload complete — Graph returns the DriveItem
                    let item: DriveItem = resp.json().await?;
                    info!("Upload complete via session: id={}", item.id);
                    return Ok(item);
                }
                202 => {
                    // Chunk accepted, continue
                    offset = end;
                    if offset >= data.len() {
                        warn!("Uploaded all bytes but got 202 — unexpected");
                        return Err(GraphError::UploadSession(
                            "unexpected 202 after final chunk".into(),
                        ));
                    }
                }
                429 => {
                    let retry = resp
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(30);
                    warn!("Rate limited during upload, sleeping {retry}s");
                    tokio::time::sleep(std::time::Duration::from_secs(retry)).await;
                }
                _ => {
                    return Err(GraphError::UploadSession(format!(
                        "Unexpected status {status} at range {range}"
                    )));
                }
            }
        }
    }

    // ── Folder / item operations ───────────────────────────────────────────────

    pub async fn create_folder(
        &self,
        parent_id: &str,
        name: &str,
    ) -> GraphResult<DriveItem> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{parent_id}/children");
        let body = CreateFolderRequest {
            name: name.to_string(),
            folder: serde_json::json!({}),
            conflict_behavior: "rename".into(),
        };

        let token = self.bearer().await?;
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        self.check_status(&resp).await?;
        let item: DriveItem = resp.json().await?;
        info!("Created folder '{name}' id={}", item.id);
        Ok(item)
    }

    pub async fn delete_item(&self, item_id: &str) -> GraphResult<()> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}");
        let token = self.bearer().await?;
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await?;
        // 204 = success for delete
        if resp.status().as_u16() == 204 || resp.status().is_success() {
            info!("Deleted item {item_id}");
            return Ok(());
        }
        self.check_status(&resp).await
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

        let token = self.bearer().await?;
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        self.check_status(&resp).await?;
        let item: DriveItem = resp.json().await?;
        info!("Moved item {item_id} -> parent={new_parent_id} name={new_name}");
        Ok(item)
    }
}
