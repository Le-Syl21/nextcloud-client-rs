// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The daemon's event loop: what `Application` and the Qt event loop do for
//! the official client.
//!
//! Everything runs on one thread (a current-thread tokio runtime with a
//! `LocalSet`), like the Qt GUI thread: the folder manager handles one
//! [`Event`] at a time. systemd is told when the daemon is ready, its
//! status, and is pinged by the watchdog from the loop itself, so a stuck
//! loop gets the service restarted.

use std::rc::Rc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::event::{Event, WatcherEvent};
use crate::folder::{FolderWatcherHandle, WatcherFactory};
use crate::folder_man::FolderMan;
use crate::folder_watcher::{FolderWatcher, FolderWatcherEvent};
use crate::timer::Timer;

const LOG: &str = "nextcloud.daemon";

impl FolderWatcherHandle for FolderWatcher {
    fn is_reliable(&self) -> bool {
        FolderWatcher::is_reliable(self)
    }

    fn can_set_permissions(&self) -> bool {
        FolderWatcher::can_set_permissions(self)
    }
}

/// The inotify folder watcher (`Folder::registerFolderWatcher`): the
/// watcher, its notification and permission self-tests, and a task
/// forwarding its signals to the queue.
pub fn inotify_watcher_factory() -> WatcherFactory {
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

/// systemd notification (no-op outside systemd).
fn notify(state: &[sd_notify::NotifyState]) {
    let _ = sd_notify::notify(state);
}

/// Posts [`Event::Shutdown`] on SIGTERM/SIGINT and
/// [`Event::ReloadCredentials`] on SIGHUP.
pub fn install_signal_handlers(tx: &UnboundedSender<Event>) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let tx = tx.clone();
    tokio::task::spawn_local(async move {
        loop {
            let event = tokio::select! {
                _ = term.recv() => Event::Shutdown,
                _ = int.recv() => Event::Shutdown,
                _ = hup.recv() => Event::ReloadCredentials,
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// Hooks of the event loop for what the folder manager does not own.
pub trait DaemonHooks {
    /// SIGHUP: re-read credentials; returns `(account id, ready)` pairs.
    fn reload_credentials(&mut self) -> Vec<(String, bool)>;
}

/// No hooks.
pub struct NoHooks;

impl DaemonHooks for NoHooks {
    fn reload_credentials(&mut self) -> Vec<(String, bool)> {
        Vec::new()
    }
}

/// How long running syncs get to abort at shutdown.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

fn status_line(fm: &FolderMan) -> String {
    let folders = fm.map().len();
    let syncing = fm.current_sync_folder().map(str::to_owned);
    let connected = fm.account_states().filter(|a| a.is_connected()).count();
    let accounts = fm.account_states().count();
    match syncing {
        Some(a) => {
            format!("{connected}/{accounts} accounts connected, {folders} folders, syncing {a}")
        }
        None => format!("{connected}/{accounts} accounts connected, {folders} folders, idle"),
    }
}

/// Runs the loop until [`Event::Shutdown`] (or the queue closes). Must run
/// inside a `LocalSet`.
pub async fn run(
    fm: &mut FolderMan,
    tx: &UnboundedSender<Event>,
    rx: &mut UnboundedReceiver<Event>,
    hooks: &mut dyn DaemonHooks,
) {
    let mut watchdog = Timer::new(Duration::ZERO, false);
    if let Some(interval) = sd_notify::watchdog_enabled() {
        log::info!(target: LOG, "systemd watchdog every {} ms", interval.as_millis());
        watchdog.start_with(interval / 2, tx, |_| Event::WatchdogTick);
    }
    notify(&[sd_notify::NotifyState::Ready]);
    let mut last_status = String::new();
    let mut shutting_down: Option<tokio::time::Instant> = None;
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
            Event::Control(request, reply) => {
                let response = crate::control::handle(fm, request);
                let _ = reply.send(response);
            }
            Event::WatchdogTick => notify(&[sd_notify::NotifyState::Watchdog]),
            Event::ReloadCredentials => {
                for (id, ready) in hooks.reload_credentials() {
                    fm.set_account_credentials_ready(&id, ready);
                }
            }
            Event::Shutdown => {
                if shutting_down.is_none() {
                    log::info!(target: LOG, "shutting down");
                    notify(&[sd_notify::NotifyState::Stopping]);
                    fm.shutdown();
                    shutting_down = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                }
            }
            e => fm.handle_event(e),
        }
        let status = status_line(fm);
        if status != last_status {
            notify(&[sd_notify::NotifyState::Status(&status)]);
            last_status = status;
        }
    }
    watchdog.stop();
}
