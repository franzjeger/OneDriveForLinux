use std::sync::Arc;
use sync_engine::SyncEngine;
use tracing::info;
use zbus::{fdo::RequestNameFlags, interface, Connection};

pub struct OneDriveInterface {
    pub engine: Arc<SyncEngine>,
}

#[interface(name = "com.onedrive.linux1")]
impl OneDriveInterface {
    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.engine.pause().await;
        info!("D-Bus: Pause");
        Ok(())
    }

    async fn resume(&self) -> zbus::fdo::Result<()> {
        self.engine.resume().await;
        info!("D-Bus: Resume");
        Ok(())
    }

    async fn get_status(&self) -> zbus::fdo::Result<Vec<(String, String)>> {
        let status = self.engine.get_status().await;
        Ok(status
            .into_iter()
            .map(|(path, state)| (path.to_string_lossy().to_string(), state.to_string()))
            .collect())
    }

    async fn force_sync(&self, path: String) -> zbus::fdo::Result<()> {
        info!("D-Bus: ForceSync {path}");
        let engine = Arc::clone(&self.engine);
        let p = std::path::PathBuf::from(path);
        tokio::spawn(async move {
            if let Err(e) = engine.upload_item(&p).await {
                tracing::error!("ForceSync error: {e}");
            }
        });
        Ok(())
    }

    async fn is_paused(&self) -> zbus::fdo::Result<bool> {
        Ok(self.engine.is_paused().await)
    }

    /// Mark a file or folder as always-on-device and download it immediately.
    async fn pin_item(&self, path: String) -> zbus::fdo::Result<()> {
        info!("D-Bus: PinItem {path}");
        let engine = Arc::clone(&self.engine);
        let p = std::path::PathBuf::from(path);
        tokio::spawn(async move {
            if let Err(e) = engine.pin_item(&p).await {
                tracing::error!("PinItem error: {e}");
            }
        });
        Ok(())
    }

    /// Remove pin from a file or folder, free cache space, convert to cloud-only.
    async fn unpin_item(&self, path: String) -> zbus::fdo::Result<()> {
        info!("D-Bus: UnpinItem {path}");
        let engine = Arc::clone(&self.engine);
        let p = std::path::PathBuf::from(path);
        tokio::spawn(async move {
            if let Err(e) = engine.unpin_item(&p).await {
                tracing::error!("UnpinItem error: {e}");
            }
        });
        Ok(())
    }
}

pub async fn export_dbus(engine: Arc<SyncEngine>) -> anyhow::Result<Connection> {
    let conn = Connection::session().await?;
    conn.object_server()
        .at("/com/onedrive/linux1", OneDriveInterface { engine })
        .await?;

    // Request the well-known name.
    // - AllowReplacement: a future daemon instance can take over from us.
    // - ReplaceExisting:  forcibly replace any current holder that has set
    //   AllowReplacement (including zombie instances of ourselves).
    let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::ReplaceExisting;
    let reply = conn
        .request_name_with_flags("com.onedrive.linux1", flags)
        .await?;

    use zbus::fdo::RequestNameReply;
    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
            info!("D-Bus interface exported at com.onedrive.linux1");
        }
        RequestNameReply::InQueue => {
            // Previous holder did NOT set AllowReplacement — wait for it to exit.
            info!("D-Bus name queued — will become active when previous holder exits");
        }
        RequestNameReply::Exists => {
            anyhow::bail!("D-Bus name com.onedrive.linux1 already taken and not replaceable");
        }
    }

    Ok(conn)
}
