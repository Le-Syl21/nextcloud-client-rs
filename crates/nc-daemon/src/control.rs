// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The control endpoint of the engine: `ncsync status|pause|resume|sync-now`
//! and the GUI's operations (decision D3 of `docs/gui-plan.md`: a GUI
//! attached to a running `ncsyncd` drives it with the same requests it
//! sends to its own engine, see [`crate::client`]).
//!
//! A Unix stream socket, named after the configuration it serves
//! ([`crate::guard::default_endpoint`]); each connection carries one
//! request, a JSON object on one line, and gets one JSON object on one line
//! back. The socket is created with mode 0600 in the daemon's runtime
//! directory, so only its user (or root) can drive it.
//!
//! Requests: `{"command":"status"}`, `{"command":"pause","folder":"1"}`,
//! `{"command":"resume"}` (no folder: all folders),
//! `{"command":"sync-now","folder":"1"}`,
//! `{"command":"reload","config":"/path/ncsyncd.cfg","wipe":["2"]}`,
//! `{"command":"selective-sync","folder":"1","exclude":["Photos/"],"include":[]}`.
//! `pause`/`resume` are `Folder::setSyncPaused`, `sync-now` is
//! `FolderMan::forceSyncForFolder` (or `scheduleAllFolders` for all),
//! `reload` re-reads the configuration ([`crate::startup::reload`], handled
//! by the event loop, answered once the removed folders stopped syncing),
//! `selective-sync` edits a folder's selective sync lists
//! (`FolderStatusModel::slotApplySelectiveSync`) and answers with its
//! blacklist (no change when both lists are empty).
//!
//! The GUI's operations change the configuration file and reload it, the
//! way the official client's dialogs change `AccountManager` and
//! `FolderMan` (handled by the event loop's
//! [`crate::daemon::DaemonHooks::control`], see [`crate::operations`]):
//! `add-account`, `start-login` / `cancel-login` (Login Flow v2),
//! `remove-account`, `sign-in` / `sign-out`, `add-folder`,
//! `remove-folder` and `set-network-limits`. `show` raises a GUI's window
//! (single instance); `ncsyncd` has none.
//!
//! `{"command":"subscribe"}` turns the connection into an event stream:
//! after `{"ok":true}`, every [`crate::ui_event::UiEvent`] is written as one
//! JSON line until the client disconnects.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::Event;
use crate::folder_man::FolderMan;
use crate::ui_event::UiEvent;

const LOG: &str = "nextcloud.daemon.control";

/// A control request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Request {
    Status,
    Pause {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
    },
    Resume {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
    },
    SyncNow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
    },
    /// Re-read the configuration.
    Reload {
        /// The configuration file the requester changed: refused when the
        /// daemon uses another one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<String>,
        /// Removed folders whose journal is deleted (`removeFolder`); the
        /// other removed folders keep theirs (`unloadFolder`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wipe: Vec<String>,
    },
    /// Exclude and include folders of the selective sync ("Choose what to
    /// sync") of a folder.
    SelectiveSync {
        /// The folder's alias or local path.
        folder: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include: Vec<String>,
    },
    /// Stream the [`crate::ui_event::UiEvent`]s on this connection.
    Subscribe,
    /// Raise the main window (a second GUI launch, like
    /// KDSingleApplication's message).
    Show,
    /// Add an account with an app password, or replace the credentials of
    /// the account with the same server and login name (`ncsync account
    /// add --app-password-file`).
    AddAccount {
        server_url: String,
        login_name: String,
        app_password: Secret,
    },
    /// Start a Login Flow v2 (`Flow2Auth`): answered with the login link
    /// and an id at once; once the user granted access, the account is
    /// added (or its credentials replaced) and
    /// [`crate::ui_event::UiEvent::LoginFinished`] is sent.
    StartLogin {
        server_url: String,
    },
    /// Stop waiting for a login started with [`Request::StartLogin`].
    CancelLogin {
        login: u64,
    },
    /// `AccountManager::deleteAccount`: the account leaves the
    /// configuration (refused while it has folders, unless `force`: they
    /// are removed too, their files kept), its app password is revoked on
    /// the server (unless the official client may share it) and its stored
    /// credentials are forgotten.
    RemoveAccount {
        account: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// `AccountState::signIn()`, then the stored credentials are read again.
    SignIn {
        account: String,
    },
    /// `AccountState::signOutByUi()`: the credentials are forgotten (also
    /// the stored ones, like `HttpCredentials::invalidateToken`) and the
    /// account signed out.
    SignOut {
        account: String,
    },
    /// "Add Folder Sync Connection" (`AccountSettings::slotFolderWizardAccepted`).
    AddFolder {
        local_path: String,
        #[serde(default = "root_path")]
        remote_path: String,
        /// The account id (may be left out when there is one account).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// "Remove folder sync connection" (`FolderMan::removeFolder`): the
    /// files are kept, the journal deleted. A folder taken over from the
    /// official client is refused unless `force` (its journal is then
    /// kept).
    RemoveFolder {
        /// The folder's alias or local path.
        folder: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// The bandwidth limits of an account ("Connection settings"); a limit
    /// left out is unchanged.
    SetNetworkLimits {
        account: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upload: Option<TransferLimit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        download: Option<TransferLimit>,
    },
}

fn root_path() -> String {
    "/".to_owned()
}

/// One direction's bandwidth limit: `Account::{upload,download}LimitSetting`
/// (`NoLimit` or `ManualLimit`) and the limit in KB/s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferLimit {
    /// `ManualLimit` when true, `NoLimit` otherwise.
    pub manual: bool,
    /// KB/s (kept even without a limit, like upstream).
    pub kbps: u32,
}

/// A secret in a request: serialized as is, never shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Request {
    /// The `command` name.
    pub fn command(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v["command"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Whether the folder manager answers the request by itself
    /// ([`handle`]); the others go to the event loop's hooks.
    pub fn is_folder_man_request(&self) -> bool {
        matches!(
            self,
            Self::Status
                | Self::Pause { .. }
                | Self::Resume { .. }
                | Self::SyncNow { .. }
                | Self::SelectiveSync { .. }
        )
    }
}

/// A control response: `{"ok":true,...}` or `{"ok":false,"error":...}`.
pub type Response = Value;

/// An error answer.
pub fn error(message: impl Into<String>) -> Response {
    json!({"ok": false, "error": message.into()})
}

/// The default socket of the daemon using `location`'s configuration:
/// see [`crate::guard::default_endpoint`].
pub fn default_socket_path(location: &crate::config_file::ConfigLocation) -> PathBuf {
    crate::guard::default_endpoint(location)
}

/// Answers a request (on the daemon's event loop).
pub fn handle(fm: &mut FolderMan, request: Request) -> Response {
    match request {
        Request::Status => status(fm),
        Request::Pause { folder } => set_paused(fm, folder.as_deref(), true),
        Request::Resume { folder } => set_paused(fm, folder.as_deref(), false),
        Request::SyncNow { folder } => sync_now(fm, folder.as_deref()),
        Request::SelectiveSync {
            folder,
            exclude,
            include,
        } => selective_sync(fm, &folder, &exclude, &include),
        // The event loop's hooks answer the others (they need the
        // configuration); a subscription is a connection of its own.
        r => error(format!("{} is not available here", r.command())),
    }
}

fn selective_sync(
    fm: &mut FolderMan,
    folder: &str,
    exclude: &[String],
    include: &[String],
) -> Response {
    let alias = match aliases(fm, Some(folder)) {
        Ok(a) => a[0].clone(),
        Err(e) => return e,
    };
    match fm.apply_selective_sync(&alias, exclude, include) {
        Some(Ok(change)) => json!({
            "ok": true,
            "folder": alias,
            "blacklist": change.black_list,
            "changes": change.changes,
            "whitelist_added": change.white_list_added,
        }),
        Some(Err(e)) => error(e.to_string()),
        None => error(format!("no such folder: {folder}")),
    }
}

fn aliases(fm: &FolderMan, folder: Option<&str>) -> Result<Vec<String>, Response> {
    match folder {
        Some(a) if fm.folder(a).is_some() => Ok(vec![a.to_owned()]),
        Some(a) => {
            // Also accept the local path of a folder.
            match fm.folder_for_path(a) {
                Some(f) if f.clean_path() == nc_sync::touched_files::clean_path(a) => {
                    Ok(vec![f.alias().to_owned()])
                }
                _ => Err(error(format!("no such folder: {a}"))),
            }
        }
        None => Ok(fm.map().keys().cloned().collect()),
    }
}

fn set_paused(fm: &mut FolderMan, folder: Option<&str>, paused: bool) -> Response {
    let aliases = match aliases(fm, folder) {
        Ok(a) => a,
        Err(e) => return e,
    };
    for alias in &aliases {
        if let Some(id) = fm.folder(alias).map(|f| f.id()) {
            if paused && let Some(f) = fm.folder_mut(alias) {
                // Pausing stops a running sync (FolderStatusModel::slotSetSyncPaused).
                f.slot_terminate_sync();
            }
            fm.set_folder_sync_paused(id, paused);
        }
    }
    json!({"ok": true, "folders": aliases})
}

fn sync_now(fm: &mut FolderMan, folder: Option<&str>) -> Response {
    let aliases = match aliases(fm, folder) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if folder.is_some() {
        fm.force_sync_for_folder(&aliases[0]);
    } else {
        fm.schedule_all_folders();
    }
    json!({"ok": true, "folders": aliases})
}

/// `status`: the accounts and the folders (a
/// [`crate::ui_event::Snapshot`] with `"ok": true`).
pub fn status(fm: &FolderMan) -> Response {
    let mut v =
        serde_json::to_value(crate::ui_event::Snapshot::of(fm)).unwrap_or_else(|_| json!({}));
    v["ok"] = Value::Bool(true);
    v
}

/// Listens on `path` and posts every request to the event queue; a
/// `subscribe` connection gets the events of `ui` (see
/// [`FolderMan::ui_sender`]).
pub fn serve(
    path: &Path,
    tx: UnboundedSender<Event>,
    ui: broadcast::Sender<UiEvent>,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // A stale socket of a previous run.
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another ncsyncd is listening on {}", path.display()),
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    log::info!(target: LOG, "control socket listening on {}", path.display());
    tokio::task::spawn_local(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let tx = tx.clone();
            let ui = ui.clone();
            tokio::task::spawn_local(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let mut reader = BufReader::new(read);
                if reader.read_line(&mut line).await.is_err() {
                    return;
                }
                let response = match serde_json::from_str::<Request>(line.trim()) {
                    Ok(Request::Subscribe) => {
                        stream_events(ui.subscribe(), &mut write).await;
                        return;
                    }
                    Ok(request) => {
                        let (otx, orx) = tokio::sync::oneshot::channel();
                        if tx.send(Event::Control(request, otx)).is_err() {
                            error("the daemon is shutting down")
                        } else {
                            orx.await
                                .unwrap_or_else(|_| error("the daemon is shutting down"))
                        }
                    }
                    Err(e) => error(format!("invalid request: {e}")),
                };
                let _ = write_line(&mut write, &response).await;
            });
        }
    });
    Ok(())
}

async fn write_line(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl Serialize,
) -> std::io::Result<()> {
    let mut text = serde_json::to_string(value).map_err(std::io::Error::other)?;
    text.push('\n');
    write.write_all(text.as_bytes()).await
}

/// A `subscribe` connection: `{"ok":true}`, then one event per line until
/// the client goes away or the engine stops.
async fn stream_events(
    mut events: broadcast::Receiver<UiEvent>,
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) {
    if write_line(write, &json!({"ok": true})).await.is_err() {
        return;
    }
    loop {
        let event = match events.recv().await {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(missed)) => UiEvent::Lagged { missed },
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if write_line(write, &event).await.is_err() {
            return;
        }
    }
}

/// Sends one request to a running daemon (`ncsync status` & co).
pub fn request(path: &Path, request: &Request) -> std::io::Result<Response> {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    // A reload waits for the syncs of removed folders to stop.
    stream.set_read_timeout(Some(crate::daemon::SHUTDOWN_GRACE * 2))?;
    let mut text = serde_json::to_string(request).map_err(std::io::Error::other)?;
    text.push('\n');
    stream.write_all(text.as_bytes())?;
    let mut line = String::new();
    std::io::BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(line.trim()).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_request_json() {
        assert_eq!(
            serde_json::to_string(&Request::SyncNow {
                folder: Some("1".into())
            })
            .unwrap(),
            r#"{"command":"sync-now","folder":"1"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"command":"pause"}"#).unwrap(),
            Request::Pause { folder: None }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"command":"status"}"#).unwrap(),
            Request::Status
        );
        assert_eq!(
            serde_json::to_string(&Request::Reload {
                config: None,
                wipe: vec!["2".into()]
            })
            .unwrap(),
            r#"{"command":"reload","wipe":["2"]}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"command":"selective-sync","folder":"1","exclude":["A"]}"#
            )
            .unwrap(),
            Request::SelectiveSync {
                folder: "1".into(),
                exclude: vec!["A".into()],
                include: Vec::new()
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"command":"add-folder","local_path":"/home/a/Nextcloud"}"#
            )
            .unwrap(),
            Request::AddFolder {
                local_path: "/home/a/Nextcloud".into(),
                remote_path: "/".into(),
                account: None
            }
        );
        assert_eq!(
            serde_json::to_string(&Request::RemoveAccount {
                account: "0".into(),
                force: false
            })
            .unwrap(),
            r#"{"command":"remove-account","account":"0"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Subscribe).unwrap(),
            r#"{"command":"subscribe"}"#
        );
        let add = Request::AddAccount {
            server_url: "https://cloud".into(),
            login_name: "a".into(),
            app_password: Secret("hunter2".into()),
        };
        assert!(
            serde_json::to_string(&add)
                .unwrap()
                .contains(r#""app_password":"hunter2""#)
        );
        assert!(!format!("{add:?}").contains("hunter2"));
        assert_eq!(add.command(), "add-account");
        assert!(!add.is_folder_man_request());
        assert!(Request::Status.is_folder_man_request());
    }
}
