// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The daemon's event loop: what `Application` and the Qt event loop do for
//! the official client.
//!
//! Everything runs on one thread (a current-thread tokio runtime with a
//! `LocalSet`), like the Qt GUI thread: the folder manager handles one
//! [`Event`] at a time. What only one embedder needs (systemd's
//! notifications and watchdog and the signal handlers of `ncsyncd`, the
//! dialogs of a GUI) goes through its [`DaemonHooks`].

use std::rc::Rc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::control::{Request, Response};
use crate::event::{Event, FolderId, WatcherEvent};
use crate::folder::{FolderWatcherHandle, WatcherFactory};
use crate::folder_man::{FolderMan, ManagerRequest};
use crate::folder_watcher::{FolderWatcher, FolderWatcherEvent};

const LOG: &str = "nextcloud.daemon";

impl FolderWatcherHandle for FolderWatcher {
    fn is_reliable(&self) -> bool {
        FolderWatcher::is_reliable(self)
    }

    fn can_set_permissions(&self) -> bool {
        FolderWatcher::can_set_permissions(self)
    }

    fn slot_lock_file_detected_externally(&self, lock_file: &str) {
        FolderWatcher::slot_lock_file_detected_externally(self, lock_file);
    }
}

/// The platform's folder watcher (`Folder::registerFolderWatcher`): inotify
/// on Linux, `ReadDirectoryChangesW` on Windows: the
/// watcher, its notification and permission self-tests, and a task
/// forwarding its signals to the queue.
pub fn default_watcher_factory() -> WatcherFactory {
    Rc::new(|id, path, files_lock_available, tx| {
        let (wtx, mut wrx) = tokio::sync::mpsc::unbounded_channel::<FolderWatcherEvent>();
        let mut watcher = FolderWatcher::new(wtx);
        let root = path.strip_suffix('/').unwrap_or(path);
        watcher.init(if root.is_empty() { "/" } else { root });
        watcher.start_notificaton_test(&format!("{path}.nextcloudsync.log"));
        watcher.perform_set_permissions_test(&format!("{path}.nextcloudpermissions.log"));
        watcher.folder_account_capabilities_changed(files_lock_available);
        let tx = tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(e) = wrx.recv().await {
                let e = match e {
                    FolderWatcherEvent::PathChanged(p) => WatcherEvent::PathChanged(p),
                    FolderWatcherEvent::FilesLockReleased(f) => {
                        WatcherEvent::FilesLockReleased(f.into_iter().collect())
                    }
                    FolderWatcherEvent::FilesLockImposed(f) => {
                        WatcherEvent::FilesLockImposed(f.into_iter().collect())
                    }
                    FolderWatcherEvent::LockedFilesFound(f) => {
                        WatcherEvent::LockedFilesFound(f.into_iter().collect())
                    }
                    FolderWatcherEvent::LostChanges => WatcherEvent::LostChanges,
                    FolderWatcherEvent::BecameUnreliable(m) => WatcherEvent::BecameUnreliable(m),
                };
                if tx.send(Event::Watcher(id, e)).is_err() {
                    return;
                }
            }
        });
        Some(Box::new(watcher) as Box<dyn FolderWatcherHandle>)
    })
}

/// The answer to a reload, and the removed folders whose sync is still
/// stopping: the answer is sent once they are unloaded.
pub struct ReloadResult {
    pub response: Response,
    pub waiting: Vec<FolderId>,
}

impl ReloadResult {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            response: serde_json::json!({"ok": false, "error": message.into()}),
            waiting: Vec::new(),
        }
    }
}

/// Hooks of the event loop for what the folder manager does not own: the
/// configuration (reloads) and what is particular to the program running
/// the loop (`ncsyncd` tells systemd, a GUI shows dialogs).
pub trait DaemonHooks {
    /// The loop starts (`ncsyncd`: `READY=1` and the watchdog timer, which
    /// posts [`Event::WatchdogTick`]).
    fn started(&mut self, _tx: &UnboundedSender<Event>) {}

    /// [`Event::WatchdogTick`]: the loop is alive.
    fn watchdog(&mut self) {}

    /// The one-line summary of the accounts and folders changed.
    fn status_changed(&mut self, _status: &str) {}

    /// [`Event::Reload`] (SIGHUP) is about to re-read the configuration.
    fn reloading(&mut self) {}

    /// The [`Event::Reload`] reload is done.
    fn reloaded(&mut self) {}

    /// [`Event::Shutdown`]: the running syncs are being aborted.
    fn stopping(&mut self) {}

    /// The loop ended.
    fn stopped(&mut self) {}

    /// SIGHUP (`config` `None`, nothing wiped) and the `reload` control
    /// request: re-reads the configuration (see
    /// [`crate::startup::reload`]). `config` is the file the requester
    /// changed; `wipe` the removed folders whose journal is deleted.
    fn reload(&mut self, fm: &mut FolderMan, config: Option<&str>, wipe: &[String])
    -> ReloadResult;

    /// A control request the folder manager does not answer by itself
    /// (see [`Request::is_folder_man_request`]): the GUI's operations on the
    /// configuration, `show`. The answer goes to `reply`, now or later (an
    /// operation may run as a task and post an [`Event::Completed`]).
    fn control(
        &mut self,
        _fm: &mut FolderMan,
        request: Request,
        _tx: &UnboundedSender<Event>,
        reply: tokio::sync::oneshot::Sender<Response>,
    ) {
        let _ = reply.send(crate::control::error(format!(
            "{} is not available here",
            request.command()
        )));
    }

    /// What the folder manager asks its owner ([`FolderMan::take_requests`],
    /// drained after every event). A GUI opens its re-authentication
    /// window for [`ManagerRequest::CredentialsNeeded`]; the folder manager
    /// already logged how to fix it and told the [`crate::ui_event::UiEvent`]
    /// subscribers.
    fn manager_request(&mut self, _fm: &mut FolderMan, _request: ManagerRequest) {}
}

/// No hooks.
pub struct NoHooks;

impl DaemonHooks for NoHooks {
    fn reload(&mut self, _: &mut FolderMan, _: Option<&str>, _: &[String]) -> ReloadResult {
        ReloadResult::error("this daemon cannot reload its configuration")
    }
}

/// A reload answer waiting for removed folders to stop syncing.
type PendingReply = (
    Vec<FolderId>,
    Response,
    tokio::sync::oneshot::Sender<Response>,
);

/// How long running syncs get to abort at shutdown.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// The one-line summary given to [`DaemonHooks::status_changed`].
pub fn status_line(fm: &FolderMan) -> String {
    let folders = fm.map().len();
    let syncing = fm.current_sync_folder().map(str::to_owned);
    let connected = fm.account_states().filter(|a| a.is_connected()).count();
    let accounts = fm.account_states().count();
    let mut line = match syncing {
        Some(a) => {
            format!("{connected}/{accounts} accounts connected, {folders} folders, syncing {a}")
        }
        None => format!("{connected}/{accounts} accounts connected, {folders} folders, idle"),
    };
    let signed_out: Vec<&str> = fm
        .account_states()
        .filter(|a| {
            matches!(
                a.state(),
                crate::account_state::State::SignedOut
                    | crate::account_state::State::AskingCredentials
            )
        })
        .map(|a| a.id())
        .collect();
    if !signed_out.is_empty() {
        line.push_str(&format!(", sign-in needed: {}", signed_out.join(", ")));
    }
    line
}

/// Runs the loop until [`Event::Shutdown`] (or the queue closes). Must run
/// inside a `LocalSet`.
pub async fn run(
    fm: &mut FolderMan,
    tx: &UnboundedSender<Event>,
    rx: &mut UnboundedReceiver<Event>,
    hooks: &mut dyn DaemonHooks,
) {
    hooks.started(tx);
    let mut last_status = String::new();
    let mut shutting_down: Option<tokio::time::Instant> = None;
    // Reload answers waiting for removed folders to stop syncing.
    let mut pending_replies: Vec<PendingReply> = Vec::new();
    loop {
        let event = match shutting_down {
            Some(deadline) => {
                if !fm.any_engine_away() {
                    break;
                }
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(e)) => e,
                    _ => {
                        log::warn!(target: LOG, "syncs did not stop in time, quitting anyway");
                        break;
                    }
                }
            }
            None => match rx.recv().await {
                Some(e) => e,
                None => break,
            },
        };
        match event {
            Event::Control(Request::Reload { config, wipe }, reply) => {
                let result = hooks.reload(fm, config.as_deref(), &wipe);
                if result.waiting.is_empty() {
                    let _ = reply.send(result.response);
                } else {
                    pending_replies.push((result.waiting, result.response, reply));
                }
            }
            Event::Control(request, reply) if request.is_folder_man_request() => {
                let response = crate::control::handle(fm, request);
                let _ = reply.send(response);
            }
            Event::Control(request, reply) => hooks.control(fm, request, tx, reply),
            Event::Completed(c) => {
                let crate::event::Completion {
                    wipe,
                    mut response,
                    reply,
                    event,
                } = *c;
                let result = hooks.reload(fm, None, &wipe);
                if response.is_object() {
                    response["reload"] = result.response;
                } else {
                    response = result.response;
                }
                if let Some(e) = event {
                    fm.publish(e);
                }
                if let Some(reply) = reply {
                    let _ = reply.send(response);
                }
            }
            Event::WatchdogTick => hooks.watchdog(),
            Event::Reload => {
                log::info!(target: LOG, "SIGHUP: reloading the configuration");
                hooks.reloading();
                let _ = hooks.reload(fm, None, &[]);
                hooks.reloaded();
            }
            Event::Shutdown => {
                if shutting_down.is_none() {
                    log::info!(target: LOG, "shutting down");
                    hooks.stopping();
                    fm.shutdown();
                    shutting_down = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                }
            }
            e => fm.handle_event(e),
        }
        for request in fm.take_requests() {
            hooks.manager_request(fm, request);
        }
        let mut i = 0;
        while i < pending_replies.len() {
            pending_replies[i].0.retain(|id| fm.is_unloading(*id));
            if pending_replies[i].0.is_empty() {
                let (_, response, reply) = pending_replies.swap_remove(i);
                let _ = reply.send(response);
            } else {
                i += 1;
            }
        }
        let status = status_line(fm);
        if status != last_status {
            hooks.status_changed(&status);
            last_status = status;
        }
    }
    hooks.stopped();
}
