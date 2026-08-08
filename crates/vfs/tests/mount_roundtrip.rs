//! The one test that exercises the whole stack: a mock Graph server, the real
//! sync engine, a real SQLite database, and a real FUSE mount.
//!
//! Every layer below has unit tests, and the layers still did not agree. Three
//! separate data-loss bugs shipped and were found by hand — an upload that
//! never left the machine, an upload that overwrote a remote change, an
//! exclusion list nothing consulted — and each one lived exactly in the seam
//! between two well-tested crates. A round trip through the mount is the only
//! thing that looks at the seams.
//!
//! What it asserts, end to end:
//!   1. A delta pass turns a Graph response into rows in the database.
//!   2. The mount shows those rows as files.
//!   3. Reading one downloads its content on demand.
//!   4. Writing one uploads it back — with `If-Match`, carrying what was
//!      written and nothing else.
//!
//! Skipped, loudly, where FUSE is unavailable (containers without `/dev/fuse`).

use graph_client::{auth::TokenSet, AuthManager, GraphClient};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sync_engine::{Config, Database, SyncEngine};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Content the mock serves for `hello.txt`, and what a read through the mount
/// must produce.
const REMOTE_CONTENT: &[u8] = b"AAAABBBB";
/// Same length as [`REMOTE_CONTENT`], so the write needs no truncate.
const WRITTEN_CONTENT: &[u8] = b"CHANGED!";
/// The version the client believes it has, and must send back as `If-Match`.
const ETAG: &str = "etag-1";
/// Content written into a file created through the mount.
const NEW_CONTENT: &[u8] = b"scaffolded by npm init\n";

/// FUSE needs `/dev/fuse` and `fusermount3`. Both are absent in some build
/// containers, where the honest outcome is "not run", not "passed".
fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
        && std::process::Command::new("fusermount3")
            .arg("--version")
            .output()
            .is_ok()
}

/// A skip is a test that did not run. That is the right answer on a machine
/// without FUSE and the wrong one in CI, where a silently skipped round trip
/// would look exactly like a passing one — so CI sets `ONEDRIVE_REQUIRE_FUSE`
/// and any skip becomes a failure.
fn skip(reason: &str) {
    if std::env::var_os("ONEDRIVE_REQUIRE_FUSE").is_some() {
        panic!("ONEDRIVE_REQUIRE_FUSE is set but the round trip could not run: {reason}");
    }
    eprintln!("SKIPPED: {reason}");
}

fn test_graph(server_uri: &str, token_dir: &Path) -> Arc<GraphClient> {
    let token = TokenSet {
        access_token: "test-token".into(),
        refresh_token: Some("refresh".into()),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        token_type: "Bearer".into(),
        scope: "Files.ReadWrite.All".into(),
    };
    let auth = Arc::new(AuthManager::for_tests(token, token_dir.join("tokens.json")));
    Arc::new(GraphClient::with_base_url(auth, server_uri.to_string()))
}

fn test_config(sync_dir: &Path) -> Config {
    Config {
        sync_dir: sync_dir.to_path_buf(),
        client_id: "test-client".into(),
        tenant_id: "common".into(),
        excluded_patterns: Config::default_excluded_patterns(),
        sync_folders: vec![],
        on_demand: true,
        max_upload_threads: 1,
        max_download_threads: 1,
        delta_poll_interval_secs: 3600,
        max_cache_size_gb: 0.0,
        auth_method: "device_code".into(),
    }
}

/// One file, `hello.txt`, in the root of a drive whose root item is `root`.
async fn mock_graph() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/me/drive/root"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "root",
            "name": "root",
            "folder": { "childCount": 1 }
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/me/drive/items/root/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{
                "id": "file1",
                "name": "hello.txt",
                "eTag": ETAG,
                "cTag": "ctag-1",
                "size": REMOTE_CONTENT.len(),
                "lastModifiedDateTime": "2026-01-01T00:00:00Z",
                "createdDateTime": "2026-01-01T00:00:00Z",
                "file": { "mimeType": "text/plain" },
                "parentReference": { "id": "root", "path": "/drive/root:" }
            }],
            "@odata.deltaLink": "https://example.invalid/delta?token=next"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/me/drive/items/file1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(REMOTE_CONTENT))
        .mount(&server)
        .await;

    // Upload target: PUT /me/drive/items/{parent}:/{name}:/content
    Mock::given(method("PUT"))
        .and(path_regex(r"^/me/drive/items/root:/hello\.txt:/content$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "file1",
            "name": "hello.txt",
            "eTag": "etag-2",
            "cTag": "ctag-2",
            "size": WRITTEN_CONTENT.len(),
            "lastModifiedDateTime": "2026-01-02T00:00:00Z",
            "file": { "mimeType": "text/plain" },
            "parentReference": { "id": "root" }
        })))
        .mount(&server)
        .await;

    server
}

/// The upload happens in a background task after `release()`, so it has not
/// necessarily reached the server by the time `close()` returns. Waits for the
/// PUT rather than sleeping a guessed interval.
async fn await_upload(server: &MockServer, within: Duration) -> wiremock::Request {
    let deadline = Instant::now() + within;
    loop {
        let requests = server.received_requests().await.unwrap_or_default();
        if let Some(req) = requests.into_iter().find(|r| r.method.as_str() == "PUT") {
            return req;
        }
        assert!(
            Instant::now() < deadline,
            "no upload reached the server within {within:?} — the edit made \
             through the mount never left the machine"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether a request with the given method reached the server in time.
async fn await_request(server: &MockServer, verb: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let requests = server.received_requests().await.unwrap_or_default();
        if requests.iter().any(|r| r.method.as_str() == verb) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_through_the_mount_reaches_onedrive() {
    if !fuse_available() {
        skip("no /dev/fuse or fusermount3 available");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let mountpoint = tmp.path().join("mount");
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&mountpoint).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();

    let server = mock_graph().await;
    let graph = test_graph(&server.uri(), tmp.path());
    let db = Arc::new(Database::open(&tmp.path().join("items.db")).unwrap());
    let config = Arc::new(test_config(&mountpoint));

    // ── 1. Delta pass: Graph response → database rows ──────────────────────
    let token = TokenSet {
        access_token: "test-token".into(),
        refresh_token: Some("refresh".into()),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        token_type: "Bearer".into(),
        scope: "Files.ReadWrite.All".into(),
    };
    let auth = Arc::new(AuthManager::for_tests(token, tmp.path().join("t2.json")));
    let (engine, _events) = SyncEngine::new(
        Arc::clone(&config),
        Arc::clone(&db),
        Arc::clone(&graph),
        auth,
        Some(cache_dir.clone()),
    );
    engine.sync_once().await.expect("delta pass");

    let item = db
        .get_item_by_id("file1")
        .await
        .unwrap()
        .expect("the delta response should have produced a row for hello.txt");
    assert_eq!(item.name, "hello.txt");
    assert!(
        item.is_placeholder,
        "on-demand mode must record the file without downloading it"
    );

    // ── 2. Mount ───────────────────────────────────────────────────────────
    let fs = vfs::OneDriveFS::new(
        Arc::clone(&db),
        Arc::clone(&graph),
        mountpoint.clone(),
        cache_dir.clone(),
        config.excluded_patterns.clone(),
    )
    .await
    .expect("build filesystem");

    let mount_handle = match fuse3::raw::Session::new(fuse3::MountOptions::default())
        .mount_with_unprivileged(fs, &mountpoint)
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            // Sandboxes permit /dev/fuse to exist but not to be mounted.
            skip(&format!("could not mount FUSE: {e}"));
            return;
        }
    };

    // Filesystem calls block the calling thread, so they must not run on the
    // runtime thread that is serving them.
    let mp = mountpoint.clone();
    let result = tokio::task::spawn_blocking(move || {
        // ── 3. The mount shows what the delta recorded ─────────────────────
        let names: Vec<String> = std::fs::read_dir(&mp)
            .expect("readdir on the mount")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"hello.txt".to_string()),
            "the mount should list the file the delta pass recorded, got {names:?}"
        );

        // ── 4. Reading downloads it on demand ──────────────────────────────
        let file = mp.join("hello.txt");
        let read = std::fs::read(&file).expect("read through the mount");
        assert_eq!(
            read, REMOTE_CONTENT,
            "reading a placeholder must fetch its content from OneDrive"
        );

        // ── 5. Writing queues an upload ────────────────────────────────────
        // Same length as the original, so this is a pure write with no
        // truncate — the narrowest path that still proves the round trip.
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .expect("open for write through the mount");
        fh.write_all(WRITTEN_CONTENT).expect("write");
        fh.flush().expect("flush");
        drop(fh);
    })
    .await;

    // Unmount before asserting, so a failed assertion cannot leave a mount
    // behind that hangs anything walking the temp directory.
    let upload = if result.is_ok() {
        Some(await_upload(&server, Duration::from_secs(15)).await)
    } else {
        None
    };
    let _ = mount_handle.unmount().await;
    result.expect("filesystem operations through the mount");

    let upload = upload.unwrap();
    assert_eq!(
        upload.body, WRITTEN_CONTENT,
        "the upload must carry what was written through the mount"
    );
    assert_eq!(
        upload
            .headers
            .get("if-match")
            .map(|v| v.to_str().unwrap_or_default()),
        Some(ETAG),
        "the upload must be conditional on the version we last synced, or a \
         change made on OneDrive meanwhile is silently overwritten"
    );
}

/// Creating a file through the mount, which the round trip above never did.
///
/// A new file gets a temporary `_local_*` ID and lives only in the cache until
/// the upload finishes. Three separate bugs lived in that window, and all three
/// showed up the first time anyone ran `npm init` inside the mount:
///
///   * `getattr` answered from the database, so a file that had just been
///     written stat'd as 0 bytes and read back empty — while `fsync()` returned
///     success. Tools that write-then-read saw an empty file.
///   * the real OneDrive ID the upload returned was thrown away, leaving a row
///     that owned the path under an ID Graph had never issued. Every later delta
///     collided with it on `local_path`.
///   * that collision rolled back the whole delta batch, so one created file
///     could stop sync recording anything at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_created_through_the_mount_is_readable_and_adopts_its_real_id() {
    if !fuse_available() {
        skip("no /dev/fuse or fusermount3 available");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let mountpoint = tmp.path().join("mount");
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&mountpoint).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();

    let server = mock_graph().await;
    // The upload of a newly created file answers with the ID OneDrive assigned.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/me/drive/items/root:/created\.txt:/content$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "real-id-from-graph",
            "name": "created.txt",
            "eTag": "etag-new",
            "cTag": "ctag-new",
            "size": NEW_CONTENT.len(),
            "lastModifiedDateTime": "2026-01-03T00:00:00Z",
            "file": { "mimeType": "text/plain" },
            "parentReference": { "id": "root" }
        })))
        .mount(&server)
        .await;

    let graph = test_graph(&server.uri(), tmp.path());
    let db = Arc::new(Database::open(&tmp.path().join("items.db")).unwrap());
    let config = Arc::new(test_config(&mountpoint));

    let token = TokenSet {
        access_token: "test-token".into(),
        refresh_token: Some("refresh".into()),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        token_type: "Bearer".into(),
        scope: "Files.ReadWrite.All".into(),
    };
    let auth = Arc::new(AuthManager::for_tests(token, tmp.path().join("t2.json")));
    let (engine, _events) = SyncEngine::new(
        Arc::clone(&config),
        Arc::clone(&db),
        Arc::clone(&graph),
        auth,
        Some(cache_dir.clone()),
    );
    // Establishes the root, so the mount has a directory to create into.
    engine.sync_once().await.expect("delta pass");

    let fs = vfs::OneDriveFS::new(
        Arc::clone(&db),
        Arc::clone(&graph),
        mountpoint.clone(),
        cache_dir.clone(),
        config.excluded_patterns.clone(),
    )
    .await
    .expect("build filesystem");

    let mount_handle = match fuse3::raw::Session::new(fuse3::MountOptions::default())
        .mount_with_unprivileged(fs, &mountpoint)
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            skip(&format!("could not mount FUSE: {e}"));
            return;
        }
    };

    let mp = mountpoint.clone();
    let result = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let file = mp.join("created.txt");

        // Exactly what a scaffolding tool does: create, write, fsync, close.
        let mut fh = std::fs::File::create(&file).expect("create through the mount");
        fh.write_all(NEW_CONTENT).expect("write");
        fh.sync_all().expect("fsync");
        drop(fh);

        // Immediately — no waiting for the upload. fsync() said the data was
        // durable, so stat and read must agree with it right now.
        let meta = std::fs::metadata(&file).expect("stat the file just written");
        assert_eq!(
            meta.len(),
            NEW_CONTENT.len() as u64,
            "a file reports {} bytes straight after a successful fsync — this is \
             what made `npm init` produce an empty package.json",
            meta.len()
        );

        let read_back = std::fs::read(&file).expect("read back what was just written");
        assert_eq!(
            read_back, NEW_CONTENT,
            "reading a just-written file must return what was written"
        );
    })
    .await;

    let upload = if result.is_ok() {
        Some(await_upload(&server, Duration::from_secs(15)).await)
    } else {
        None
    };
    let _ = mount_handle.unmount().await;
    result.expect("filesystem operations through the mount");
    assert_eq!(
        upload.unwrap().body,
        NEW_CONTENT,
        "the created file must reach OneDrive with its content"
    );

    // ── The real ID must be adopted ────────────────────────────────────────
    // Poll: the database write happens in the upload's background task.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if db
            .get_item_by_id("real-id-from-graph")
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the row still holds the temporary _local_ ID after the upload \
             returned a real one — every later delta will collide with it on \
             local_path"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ── And a later delta must be able to record over it ────────────────────
    // This is the failure that stopped sync entirely: the collision aborted the
    // whole batch, not just the one row.
    db.upsert_items_batch(vec![sync_engine::DbItem {
        id: "real-id-from-graph".into(),
        local_path: mountpoint.join("created.txt"),
        name: "created.txt".into(),
        parent_id: Some("root".into()),
        etag: Some("etag-new".into()),
        ctag: None,
        size: NEW_CONTENT.len() as u64,
        modified_at: None,
        created_at: None,
        sha1_hash: None,
        quick_xor_hash: None,
        is_folder: false,
        is_placeholder: false,
        sync_state: sync_engine::SyncState::Synced,
        pinned: false,
    }])
    .await
    .expect("a delta pass must be able to record a file created through the mount");
}

/// Deleting a file within seconds of writing it did not take: the file came
/// back, with its content.
///
/// A locally created file uploads in the background after `release()`. `unlink()`
/// deleted the row and the cached copy, but the upload was already in flight —
/// and on completion it re-read the row, found it gone, fell back to its own
/// stale copy and wrote it back. Meanwhile the upload had created the file on
/// OneDrive, so the resurrected row pointed at real remote content.
///
/// `unlink()` could not have prevented it. For a `_local_*` item there is
/// nothing on OneDrive to delete at the time it runs; the remote copy only comes
/// into existence when the upload finishes. Removing it belongs to the upload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_file_while_its_upload_is_in_flight_makes_the_delete_win() {
    if !fuse_available() {
        skip("no /dev/fuse or fusermount3 available");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let mountpoint = tmp.path().join("mount");
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&mountpoint).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();

    let server = mock_graph().await;
    // Held open long enough that the unlink lands mid-upload every run, rather
    // than depending on how fast the machine is.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/me/drive/items/root:/churn\.bin:/content$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(serde_json::json!({
                    "id": "uploaded-then-deleted",
                    "name": "churn.bin",
                    "eTag": "etag-churn",
                    "size": NEW_CONTENT.len(),
                    "lastModifiedDateTime": "2026-01-04T00:00:00Z",
                    "file": { "mimeType": "application/octet-stream" },
                    "parentReference": { "id": "root" }
                })),
        )
        .mount(&server)
        .await;
    // The upload must clean up after itself.
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/uploaded-then-deleted"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let graph = test_graph(&server.uri(), tmp.path());
    let db = Arc::new(Database::open(&tmp.path().join("items.db")).unwrap());
    let config = Arc::new(test_config(&mountpoint));

    let token = TokenSet {
        access_token: "test-token".into(),
        refresh_token: Some("refresh".into()),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        token_type: "Bearer".into(),
        scope: "Files.ReadWrite.All".into(),
    };
    let auth = Arc::new(AuthManager::for_tests(token, tmp.path().join("t2.json")));
    let (engine, _events) = SyncEngine::new(
        Arc::clone(&config),
        Arc::clone(&db),
        Arc::clone(&graph),
        auth,
        Some(cache_dir.clone()),
    );
    engine.sync_once().await.expect("delta pass");

    let fs = vfs::OneDriveFS::new(
        Arc::clone(&db),
        Arc::clone(&graph),
        mountpoint.clone(),
        cache_dir.clone(),
        config.excluded_patterns.clone(),
    )
    .await
    .expect("build filesystem");

    let mount_handle = match fuse3::raw::Session::new(fuse3::MountOptions::default())
        .mount_with_unprivileged(fs, &mountpoint)
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            skip(&format!("could not mount FUSE: {e}"));
            return;
        }
    };

    let mp = mountpoint.clone();
    let result = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let file = mp.join("churn.bin");
        let mut fh = std::fs::File::create(&file).expect("create");
        fh.write_all(NEW_CONTENT).expect("write");
        drop(fh);
        // Immediately, inside the upload window — the case that failed.
        std::fs::remove_file(&file).expect("delete right after writing");
        assert!(
            !file.exists(),
            "the file must be gone the moment unlink() returns"
        );
    })
    .await;

    // Outlast the upload, so the completion path has run.
    let outcome = if result.is_ok() {
        let deleted = await_request(&server, "DELETE", Duration::from_secs(15)).await;
        // And it must still be gone once everything has settled.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let row = db.get_item_by_id("uploaded-then-deleted").await.unwrap();
        let still_there = mountpoint.join("churn.bin");
        Some((deleted, row, still_there.exists()))
    } else {
        None
    };
    let _ = mount_handle.unmount().await;
    result.expect("filesystem operations through the mount");

    let (deleted, row, path_exists) = outcome.unwrap();
    assert!(
        deleted,
        "the upload created the file on OneDrive after the local delete and never \
         removed it — the file comes back on the next delta, and a rewrite of the \
         same name gets 409 nameAlreadyExists"
    );
    assert!(
        row.is_none(),
        "the completed upload wrote the deleted file's row back — this is the \
         resurrection: delete a file within seconds of writing it and it returns"
    );
    assert!(!path_exists, "the deleted file reappeared in the mount");
}
