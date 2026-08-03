//! Desktop notifications for the few events that genuinely need attention.
//!
//! Sent over the standard `org.freedesktop.Notifications` interface rather than
//! through another dependency — the daemon already holds a session bus
//! connection for its own interface.
//!
//! The bar for notifying is deliberately high: a sync client that pops up a
//! banner per file is one people turn off, and then they miss the notification
//! that mattered. Only three things qualify — sign-in expired, an upload was
//! given up on, and a conflicting edit was set aside — because each one means
//! the app cannot do what the user asked without them.

use tracing::{debug, warn};
use zbus::Connection;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP_NAME: &str = "OneDrive";
/// Matches the installed launcher icon, so banners carry the app's own icon.
const APP_ICON: &str = "onedrive-linux";

/// How long a banner stays up. Errors persist until dismissed (0); the rest
/// use the desktop's default (-1).
#[derive(Clone, Copy)]
pub enum Urgency {
    Normal,
    Critical,
}

impl Urgency {
    fn timeout_ms(self) -> i32 {
        match self {
            Urgency::Normal => -1,
            Urgency::Critical => 0,
        }
    }

    fn hint(self) -> u8 {
        match self {
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        }
    }
}

/// Show a desktop notification. Never fails the caller: a missing notification
/// daemon (a bare WM, a headless session) is normal, not an error worth
/// interrupting sync over.
pub async fn notify(conn: &Connection, summary: &str, body: &str, urgency: Urgency) {
    let mut hints = std::collections::HashMap::new();
    hints.insert("urgency", zbus::zvariant::Value::U8(urgency.hint()));

    let result = conn
        .call_method(
            Some(SERVICE),
            PATH,
            Some(SERVICE),
            "Notify",
            &(
                APP_NAME,
                0u32, // replaces_id: 0 = new notification
                APP_ICON,
                summary,
                body,
                Vec::<String>::new(), // actions
                hints,
                urgency.timeout_ms(),
            ),
        )
        .await;

    match result {
        Ok(_) => debug!("Notification sent: {summary}"),
        Err(e) => debug!("Could not send notification ({summary}): {e}"),
    }
}

/// Watch sync events and notify on the ones that need the user.
pub fn spawn(conn: Connection, mut rx: tokio::sync::broadcast::Receiver<sync_engine::SyncEvent>) {
    tokio::spawn(async move {
        // Conflicts arrive as ordinary item-state changes and can come in
        // bursts after a long offline period; one banner per pass is enough to
        // tell the user to go look.
        let mut conflict_announced = false;

        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    debug!("Notifier lagged {n} events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            match event {
                sync_engine::SyncEvent::AuthRequired => {
                    notify(
                        &conn,
                        "OneDrive needs you to sign in",
                        "Syncing is paused until you sign in again. Open OneDrive to continue.",
                        Urgency::Critical,
                    )
                    .await;
                }
                sync_engine::SyncEvent::UploadFailed { name, error } => {
                    warn!("Upload permanently failed: {name}: {error}");
                    notify(
                        &conn,
                        "OneDrive could not upload a file",
                        &format!(
                            "{name} could not be uploaded after several attempts. \
                             Your local copy is unchanged.\n{error}"
                        ),
                        Urgency::Critical,
                    )
                    .await;
                }
                sync_engine::SyncEvent::ItemStateChanged {
                    path,
                    state: sync_engine::SyncState::Conflict,
                } => {
                    if !conflict_announced {
                        conflict_announced = true;
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.to_string_lossy().to_string());
                        notify(
                            &conn,
                            "OneDrive found a conflicting edit",
                            &format!(
                                "{name} was changed in both places. Your version was kept \
                                 alongside the one from OneDrive, with the time added to \
                                 its name."
                            ),
                            Urgency::Normal,
                        )
                        .await;
                    }
                }
                sync_engine::SyncEvent::SyncCompleted => conflict_announced = false,
                _ => {}
            }
        }
    });
}
