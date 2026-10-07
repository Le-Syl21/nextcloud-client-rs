// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The control socket of `ncsyncd`: `ncsync status|pause|resume|sync-now`.
//!
//! A Unix stream socket; each connection carries one request, a JSON object
//! on one line, and gets one JSON object on one line back. The socket is
//! created with mode 0600 in the daemon's runtime directory, so only its
//! user (or root) can drive it.
//!
//! Requests: `{"command":"status"}`, `{"command":"pause","folder":"1"}`,
//! `{"command":"resume"}` (no folder: all folders),
//! `{"command":"sync-now","folder":"1"}`.
//! `pause`/`resume` are `Folder::setSyncPaused`, `sync-now` is
//! `FolderMan::forceSyncForFolder` (or `scheduleAllFolders` for all).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::Event;
use crate::folder_man::FolderMan;

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
}

/// A control response: `{"ok":true,...}` or `{"ok":false,"error":...}`.
pub type Response = Value;

fn error(message: impl Into<String>) -> Response {
    json!({"ok": false, "error": message.into()})
}

/// The default socket path: `$XDG_RUNTIME_DIR/ncsyncd/control.sock` for
/// a user daemon, `/run/ncsyncd/<instance>/control.sock` for an instance of
/// the system service.
pub fn default_socket_path(instance: Option<&str>) -> PathBuf {
    if let Some(i) = instance {
        return PathBuf::from(format!("/run/ncsyncd/{i}/control.sock"));
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("/tmp/ncsyncd-{}", rustix::process::getuid().as_raw()));
    PathBuf::from(base).join("ncsyncd").join("control.sock")
}

/// Answers a request (on the daemon's event loop).
pub fn handle(fm: &mut FolderMan, request: Request) -> Response {
    match request {
        Request::Status => status(fm),
        Request::Pause { folder } => set_paused(fm, folder.as_deref(), true),
        Request::Resume { folder } => set_paused(fm, folder.as_deref(), false),
        Request::SyncNow { folder } => sync_now(fm, folder.as_deref()),
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

fn secs(t: Option<std::time::SystemTime>) -> Value {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(Value::Null, |d| json!(d.as_secs()))
}

/// `status`: the accounts and the folders.
pub fn status(fm: &FolderMan) -> Response {
    let accounts: Vec<Value> = fm
        .account_states()
        .map(|a| {
            json!({
                "id": a.id(),
                "url": a.account().url().to_credential_free_string(),
                "user": a.account().dav_user(),
                "state": a.state().name(),
                "connection_status": a.connection_status().name(),
                "errors": a.connection_errors(),
                "server_version": a.account().server_version(),
            })
        })
        .collect();
    let queue = fm.schedule_queue();
    let current = fm.current_sync_folder().map(str::to_owned);
    let folders: Vec<Value> = fm
        .map()
        .values()
        .map(|f| {
            let r = f.sync_result();
            let progress = f.progress().map(|p| {
                json!({
                    "status": format!("{:?}", p.status),
                    "completed_files": p.completed_files(),
                    "total_files": p.total_files(),
                    "completed_size": p.completed_size(),
                    "total_size": p.total_size(),
                })
            });
            json!({
                "alias": f.alias(),
                "account": f.account_id(),
                "local_path": f.path(),
                "remote_path": f.remote_path(),
                "paused": f.sync_paused(),
                "syncing": f.is_sync_running(),
                "scheduled": queue.iter().any(|q| q == f.alias()),
                "current": current.as_deref() == Some(f.alias()),
                "status": r.status().name(),
                "status_string": r.status_string(),
                "last_status_time": secs(r.sync_time()),
                "errors": r.error_strings(),
                "consecutive_failing_syncs": f.consecutive_failing_syncs(),
                "seconds_since_last_sync": f.msec_since_last_sync().as_secs(),
                "last_sync_duration_ms": f.msec_last_sync_duration().as_millis() as u64,
                "unresolved_conflicts": r.has_unresolved_conflicts(),
                "watcher": f.has_folder_watcher(),
                "progress": progress,
            })
        })
        .collect();
    json!({"ok": true, "accounts": accounts, "folders": folders})
}

/// Listens on `path` and posts every request to the event queue.
pub fn serve(path: &Path, tx: UnboundedSender<Event>) -> std::io::Result<()> {
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
            tokio::task::spawn_local(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let mut reader = BufReader::new(read);
                if reader.read_line(&mut line).await.is_err() {
                    return;
                }
                let response = match serde_json::from_str::<Request>(line.trim()) {
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
                let mut text = response.to_string();
                text.push('\n');
                let _ = write.write_all(text.as_bytes()).await;
            });
        }
    });
    Ok(())
}

/// Sends one request to a running daemon (`ncsync status` & co).
pub fn request(path: &Path, request: &Request) -> std::io::Result<Response> {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
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
    }
}
