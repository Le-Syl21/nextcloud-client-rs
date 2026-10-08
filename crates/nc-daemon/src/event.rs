// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The daemon's event queue: what Qt delivers through its event loop
//! (timer timeouts, queued signal emissions, finished network jobs) is an
//! [`Event`] posted to one unbounded channel and handled one at a time by
//! the folder manager, in posting order.

use std::time::SystemTime;

use nc_dav::HttpError;
use nc_sync::{ErrorCategory, ProgressInfo, SyncEngine, SyncFileItemPtr};

use crate::account_state::AccountEvent;

/// Identifies one `Folder` object (aliases can be reused after a removal).
pub type FolderId = u64;

/// The daemon's events.
pub enum Event {
    /// An event of the account state `0`.
    Account(String, AccountEvent),
    /// An event of the folder manager.
    FolderMan(FolderManEvent),
    /// An event of a folder.
    Folder(FolderId, FolderEvent),
    /// A signal of a folder's watcher.
    Watcher(FolderId, WatcherEvent),
    /// A signal of an account's push notifications.
    Push(String, PushEvent),
    /// A request on the control socket, with where to send the answer.
    Control(
        crate::control::Request,
        tokio::sync::oneshot::Sender<crate::control::Response>,
    ),
    /// The systemd watchdog interval passed: the loop is alive.
    WatchdogTick,
    /// Reload the credentials of the accounts waiting for them (SIGHUP).
    ReloadCredentials,
    /// Stop: abort the running syncs and quit.
    Shutdown,
}

/// `FolderMan` timers and queued invocations.
#[derive(Debug)]
pub enum FolderManEvent {
    /// `_etagPollTimer` timeout.
    EtagPollTimer(u64),
    /// `_timeScheduler` timeout.
    TimeScheduler(u64),
    /// `_startScheduledSyncTimer` timeout.
    StartScheduledSyncTimer(u64),
    /// `QMetaObject::invokeMethod(this, "slotRunOneEtagJob", Qt::QueuedConnection)`.
    RunOneEtagJob,
    /// `QTimer::singleShot(syncAgainDelay, ...)` of `scheduleFolder`: enqueue
    /// the folder now.
    DelayedEnqueue(FolderId),
    /// `QTimer::singleShot(syncAgainDelay, ...)` of `scheduleFolder` for an
    /// already queued folder.
    DelayedStartSoon,
}

/// `Folder` timers, engine signals and job results.
pub enum FolderEvent {
    /// `_scheduleSelfTimer` timeout.
    ScheduleSelfTimer(u64),
    /// `QTimer::singleShot(200, this, &Folder::slotEmitFinishedDelayed)`.
    EmitFinishedDelayed,
    /// `QMetaObject::invokeMethod(folder, "slotRunEtagJob", Qt::QueuedConnection)`.
    RunEtagJob,
    /// `RequestEtagJob` finished (`etagRetrieved(etag, time)` or an error).
    EtagJobFinished(Result<Vec<u8>, HttpError>, SystemTime),
    /// The engine's scheduled sync run timer fired (lock expiry).
    ScheduledSyncTimer(u64),
    /// A `setLockFileState` request of the folder finished (the account's
    /// `lockFileSuccess()` / `lockFileError(message)`).
    LockFileStateFinished(LockFileStateFinished),
    /// `SyncEngine::lockFileDetected(lockFile)`.
    LockFileDetected(String),
    /// `SyncEngine::started()`.
    EngineStarted,
    /// `SyncEngine::itemCompleted(item, category)`.
    ItemCompleted(SyncFileItemPtr, ErrorCategory),
    /// `SyncEngine::transmissionProgress(progress)`.
    TransmissionProgress(Box<ProgressInfo>),
    /// `SyncEngine::syncError(message, category)`.
    SyncError(String, ErrorCategory),
    /// `SyncEngine::rootEtag(etag, time)`.
    RootEtag(Vec<u8>, SystemTime),
    /// `SyncEngine::rootFileIdReceived(fileId)`.
    RootFileId(i64),
    /// `SyncEngine::newBigFolder(folder, isExternal)`.
    NewBigFolder(String, bool),
    /// `SyncEngine::existingFolderNowBig(folder)`.
    ExistingFolderNowBig(String),
    /// The mass deletion prompt was answered "restore" (see
    /// `Folder::slotAboutToRemoveAllFiles`).
    AllFilesDeletedRestore,
    /// `SyncEngine::finished(success)`: the engine comes back to the folder.
    EngineFinished(Box<SyncEngine>, bool),
}

/// The end of a `setLockFileState` request made by a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockFileStateFinished {
    /// The server-relative path of the file.
    pub remote_file_path: String,
    /// The requested state: true for a lock, false for an unlock.
    pub lock: bool,
    /// `Ok` for `lockFileSuccess()`, the message of `lockFileError`.
    pub result: Result<(), String>,
}

/// The signals of `FolderWatcher`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatcherEvent {
    /// `pathChanged(path)`: an absolute path.
    PathChanged(String),
    /// `lostChanges()`: the next sync must do a full local discovery.
    LostChanges,
    /// `becameUnreliable(message)`.
    BecameUnreliable(String),
    /// `filesLockReleased(files)`.
    FilesLockReleased(Vec<String>),
    /// `filesLockImposed(files)`.
    FilesLockImposed(Vec<String>),
    /// `lockedFilesFound(files)`.
    LockedFilesFound(Vec<String>),
}

/// The signals of `PushNotifications` / `Account` the folder manager and the
/// account state listen to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushEvent {
    /// `Account::pushNotificationsReady(account)`.
    Ready,
    /// `PushNotifications::filesChanged(account)`.
    FilesChanged,
    /// `PushNotifications::fileIdsChanged(account, fileIds)`.
    FileIdsChanged(Vec<i64>),
    /// `Account::pushNotificationsDisabled(account)`: the push connection
    /// is gone (failed setup, connection lost, authentication failed).
    Disabled,
}
