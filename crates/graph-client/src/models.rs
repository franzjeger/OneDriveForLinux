use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Represents a file or folder in Microsoft OneDrive (Graph API DriveItem).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    /// Not present for deleted items in delta responses.
    #[serde(default)]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_date_time: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified_date_time: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<FileMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<FolderMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_reference: Option<ItemReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_system_info: Option<FileSystemInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// Present when an item has been deleted in a delta response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<DeletedMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// Present on items that live in a different drive (e.g. Teams Chat Files).
    /// We can't sync these via the personal OneDrive API, so we skip them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_item: Option<serde_json::Value>,
}

impl DriveItem {
    pub fn is_folder(&self) -> bool {
        self.folder.is_some()
    }

    pub fn is_file(&self) -> bool {
        self.file.is_some()
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    /// True for items that live in a remote drive (e.g. Teams Chat Files shortcuts).
    /// These cannot be downloaded via the personal OneDrive API and must be skipped.
    pub fn is_remote_item(&self) -> bool {
        self.remote_item.is_some()
    }

    /// True for the drive root item (present in every delta response, maps to sync_dir itself).
    pub fn is_root(&self) -> bool {
        self.parent_reference
            .as_ref()
            .map(|r| r.id.is_none())
            .unwrap_or(false)
    }

    pub fn sha1_hash(&self) -> Option<&str> {
        self.file
            .as_ref()
            .and_then(|f| f.hashes.as_ref())
            .and_then(|h| h.sha1_hash.as_deref())
    }

    pub fn quick_xor_hash(&self) -> Option<&str> {
        self.file
            .as_ref()
            .and_then(|f| f.hashes.as_ref())
            .and_then(|h| h.quick_xor_hash.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMetadata {
    pub child_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha1_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_xor_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemReference {
    pub id: Option<String>,
    pub drive_id: Option<String>,
    pub path: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    pub created_date_time: Option<DateTime<Utc>>,
    pub last_modified_date_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedMetadata {
    pub state: Option<String>,
}

/// Response from Graph API delta endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaResponse {
    #[serde(rename = "value")]
    pub items: Vec<DriveItem>,
    /// Link to get the next page of results.
    #[serde(rename = "@odata.nextLink", skip_serializing_if = "Option::is_none")]
    pub next_link: Option<String>,
    /// Link to use on the next delta poll.
    #[serde(rename = "@odata.deltaLink", skip_serializing_if = "Option::is_none")]
    pub delta_link: Option<String>,
}

/// An upload session for large file uploads (>4 MB).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    pub upload_url: String,
    pub expiration_date_time: Option<DateTime<Utc>>,
    pub next_expected_ranges: Option<Vec<String>>,
}

/// Payload for creating an upload session.
#[derive(Debug, Serialize)]
pub struct CreateUploadSessionRequest {
    pub item: UploadSessionItem,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSessionItem {
    #[serde(rename = "@microsoft.graph.conflictBehavior")]
    pub conflict_behavior: String,
    pub name: String,
}

/// Payload for creating a folder.
#[derive(Debug, Serialize)]
pub struct CreateFolderRequest {
    pub name: String,
    pub folder: serde_json::Value,
    #[serde(rename = "@microsoft.graph.conflictBehavior")]
    pub conflict_behavior: String,
}

/// Payload for moving/renaming an item.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveItemRequest {
    pub parent_reference: MoveParentReference,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct MoveParentReference {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_regular_file_item() {
        let item: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "item1",
            "name": "doc.txt",
            "eTag": "e1",
            "size": 42,
            "file": {"mimeType": "text/plain", "hashes": {"sha1Hash": "abc"}},
            "parentReference": {"id": "parent1", "path": "/drive/root:/Docs"}
        }))
        .unwrap();
        assert!(item.is_file());
        assert!(!item.is_folder());
        assert!(!item.is_deleted());
        assert!(!item.is_root());
        assert_eq!(item.sha1_hash(), Some("abc"));
        assert_eq!(item.size, Some(42));
    }

    #[test]
    fn parses_deleted_item_without_name() {
        // Delta responses omit `name` for deleted items — must not fail to parse.
        let item: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "gone1",
            "deleted": {"state": "deleted"}
        }))
        .unwrap();
        assert!(item.is_deleted());
        assert_eq!(item.name, "");
    }

    #[test]
    fn root_item_is_detected_by_missing_parent_id() {
        let root: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "root1",
            "name": "root",
            "folder": {"childCount": 3},
            "parentReference": {}
        }))
        .unwrap();
        assert!(root.is_root());

        let child: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "c1",
            "name": "x",
            "folder": {},
            "parentReference": {"id": "root1"}
        }))
        .unwrap();
        assert!(!child.is_root());
    }

    #[test]
    fn remote_item_is_flagged() {
        let item: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "r1",
            "name": "Teams Chat Files",
            "remoteItem": {"id": "other"}
        }))
        .unwrap();
        assert!(item.is_remote_item());
    }

    #[test]
    fn delta_response_links_parse() {
        let resp: DeltaResponse = serde_json::from_value(serde_json::json!({
            "value": [],
            "@odata.deltaLink": "https://example/delta?token=t"
        }))
        .unwrap();
        assert!(resp.items.is_empty());
        assert!(resp.next_link.is_none());
        assert_eq!(
            resp.delta_link.as_deref(),
            Some("https://example/delta?token=t")
        );
    }
}
