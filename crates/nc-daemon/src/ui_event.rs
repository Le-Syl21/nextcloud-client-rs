// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! What the engine tells an embedding GUI: the [`UiEvent`] stream and the
//! state snapshots it carries.
//!
//! Upstream, the GUI classes connect to the signals of `FolderMan`,
//! `Folder`, `AccountState` and `ProgressDispatcher`. Here the folder
//! manager publishes the same moments on a `tokio::sync::broadcast`
//! channel ([`crate::folder_man::FolderMan::subscribe`]). The types are
//! serde types, so the same stream goes over the control endpoint to a GUI
//! attached to a running `ncsyncd` (see [`crate::client`]).
//!
//! A subscriber that falls behind gets [`UiEvent::Lagged`]: it then asks
//! for the status again ([`crate::control::Request::Status`], answered with
//! a [`Snapshot`]).

use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::account_state::{AccountState, State};
use crate::folder::Folder;
use crate::folder_man::FolderMan;
use crate::sync_result::SyncStatus;

/// How many events a subscriber may fall behind before it misses some.
pub const UI_EVENT_CAPACITY: usize = 1024;

/// `ProgressInfo::Status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressPhase {
    #[default]
    Starting,
    Discovery,
    Reconcile,
    Propagation,
    Done,
}

impl From<nc_sync::ProgressStatus> for ProgressPhase {
    fn from(s: nc_sync::ProgressStatus) -> Self {
        use nc_sync::ProgressStatus as P;
        match s {
            P::Starting => Self::Starting,
            P::Discovery => Self::Discovery,
            P::Reconcile => Self::Reconcile,
            P::Propagation => Self::Propagation,
            P::Done => Self::Done,
        }
    }
}

/// What a GUI shows of a running sync (`ProgressInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSummary {
    pub status: ProgressPhase,
    pub completed_files: i64,
    pub total_files: i64,
    pub completed_size: i64,
    pub total_size: i64,
    /// `currentFile()`: the number of the file being transferred.
    #[serde(default)]
    pub current_file: i64,
    /// `totalProgress().estimatedEta` in milliseconds when `trustEta()`;
    /// `None` when there is no trusted estimate. The port's `ProgressInfo`
    /// keeps no transfer-rate estimates yet, so this is always `None`.
    #[serde(default)]
    pub eta_msecs: Option<u64>,
}

impl ProgressSummary {
    pub fn of(p: &nc_sync::ProgressInfo) -> Self {
        Self {
            status: p.status.into(),
            completed_files: p.completed_files(),
            total_files: p.total_files(),
            completed_size: p.completed_size(),
            total_size: p.total_size(),
            current_file: p.current_file(),
            eta_msecs: None,
        }
    }
}

/// An account as a GUI shows it (the `status` answer, `AccountState`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub id: String,
    /// The server URL, without credentials.
    pub url: String,
    /// The DAV user.
    pub user: String,
    /// `Account::displayName()`: `user@host`.
    #[serde(default)]
    pub display_name: String,
    pub state: State,
    /// `AccountState::stateString(state)`.
    #[serde(default)]
    pub state_string: String,
    /// `isConnected()`.
    #[serde(default)]
    pub connected: bool,
    /// `isSignedOut()`.
    #[serde(default)]
    pub signed_out: bool,
    /// The last `ConnectionValidator::Status` (its `Q_ENUM` key).
    pub connection_status: String,
    /// `connectionErrors()`.
    pub errors: Vec<String>,
    pub server_version: String,
}

impl AccountSnapshot {
    pub fn of(a: &AccountState) -> Self {
        let account = a.account();
        Self {
            id: a.id().to_owned(),
            url: account.url().to_credential_free_string(),
            user: account.dav_user(),
            display_name: crate::account_setup::display_name(
                &account.credentials().user,
                account.url(),
            ),
            state: a.state(),
            state_string: a.state().state_string().to_owned(),
            connected: a.is_connected(),
            signed_out: a.is_signed_out(),
            connection_status: a.connection_status().name().to_owned(),
            errors: a.connection_errors().to_vec(),
            server_version: account.server_version(),
        }
    }
}

/// A folder as a GUI shows it (the `status` answer, `Folder` and its
/// `SyncResult`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSnapshot {
    pub alias: String,
    /// The account id.
    pub account: String,
    pub local_path: String,
    pub remote_path: String,
    /// `syncPaused()`.
    pub paused: bool,
    /// `isSyncRunning()`.
    pub syncing: bool,
    /// In the folder manager's queue.
    pub scheduled: bool,
    /// The folder manager's `currentSyncFolder()`.
    pub current: bool,
    /// `syncResult().status()`.
    pub status: SyncStatus,
    /// `syncResult().statusString()`.
    pub status_string: String,
    /// When the status was set (seconds since the epoch).
    pub last_status_time: Option<u64>,
    /// `syncResult().errorStrings()`.
    pub errors: Vec<String>,
    pub consecutive_failing_syncs: u32,
    pub seconds_since_last_sync: u64,
    pub last_sync_duration_ms: u64,
    /// `syncResult().hasUnresolvedConflicts()`.
    pub unresolved_conflicts: bool,
    /// The folder has a working folder watcher.
    pub watcher: bool,
    /// The progress of the running (or last) sync.
    pub progress: Option<ProgressSummary>,
}

impl FolderSnapshot {
    pub fn of(fm: &FolderMan, f: &Folder) -> Self {
        let r = f.sync_result();
        Self {
            alias: f.alias().to_owned(),
            account: f.account_id().to_owned(),
            local_path: f.path().to_owned(),
            remote_path: f.remote_path().to_owned(),
            paused: f.sync_paused(),
            syncing: f.is_sync_running(),
            scheduled: fm.is_scheduled(f.id()),
            current: fm.current_sync_folder() == Some(f.alias()),
            status: r.status(),
            status_string: r.status_string().to_owned(),
            last_status_time: r
                .sync_time()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            errors: r.error_strings().to_vec(),
            consecutive_failing_syncs: f.consecutive_failing_syncs(),
            seconds_since_last_sync: f.msec_since_last_sync().as_secs(),
            last_sync_duration_ms: f.msec_last_sync_duration().as_millis() as u64,
            unresolved_conflicts: r.has_unresolved_conflicts(),
            watcher: f.has_folder_watcher(),
            progress: f.progress().map(ProgressSummary::of),
        }
    }
}

/// Every account and folder: the `status` answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub accounts: Vec<AccountSnapshot>,
    pub folders: Vec<FolderSnapshot>,
}

impl Snapshot {
    pub fn of(fm: &FolderMan) -> Self {
        Self {
            accounts: fm.account_states().map(AccountSnapshot::of).collect(),
            folders: fm
                .map()
                .values()
                .map(|f| FolderSnapshot::of(fm, f))
                .collect(),
        }
    }

    /// Reads the answer of a `status` request.
    pub fn from_response(response: &serde_json::Value) -> Option<Self> {
        if response["ok"] != true {
            return None;
        }
        serde_json::from_value(response.clone()).ok()
    }
}

/// A completed item (`SyncEngine::itemCompleted`), for the activity list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedItem {
    /// The path relative to the folder.
    pub file: String,
    /// The rename target, empty when it is not a rename.
    #[serde(default)]
    pub rename_target: String,
    /// `SyncInstructions` (the variant name: `New`, `Remove`, `Sync`, ...).
    pub instruction: String,
    /// `SyncFileItem::Direction` (`None`, `Up`, `Down`).
    pub direction: String,
    /// `SyncFileItem::Status` (`Success`, `Conflict`, `NormalError`, ...).
    pub status: String,
    /// `SyncFileItem::_errorString`.
    #[serde(default)]
    pub error_string: String,
    pub size: i64,
    /// `ErrorCategory` (`NoError`, `GenericError`, ...).
    pub category: String,
}

impl CompletedItem {
    pub fn of(item: &nc_sync::SyncFileItem, category: nc_sync::ErrorCategory) -> Self {
        Self {
            file: item.file.clone(),
            rename_target: item.rename_target.clone(),
            instruction: format!("{:?}", item.instruction),
            direction: format!("{:?}", item.direction),
            status: format!("{:?}", item.status),
            error_string: item.error_string.clone(),
            size: item.size,
            category: format!("{category:?}"),
        }
    }
}

/// The events of the engine for a GUI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum UiEvent {
    /// An account was added, or its state or connection errors changed
    /// (`AccountState::stateChanged`).
    AccountStateChanged { account: AccountSnapshot },
    /// An account was removed (`AccountManager::accountRemoved`).
    AccountRemoved { account: String },
    /// The account needs new credentials: upstream's `askFromUser()`, where
    /// the GUI opens the browser re-authentication window.
    CredentialsNeeded { account: String },
    /// A folder was added, queued, or its sync state changed
    /// (`Folder::syncStateChange`, `syncPausedChanged`, `syncFinished`).
    FolderStateChanged { folder: FolderSnapshot },
    /// A folder was removed or unloaded.
    FolderRemoved { folder: String },
    /// `ProgressDispatcher::progressInfo(folder, progress)`.
    Progress {
        folder: String,
        progress: ProgressSummary,
    },
    /// `SyncEngine::itemCompleted(item)` (feeds the activity list).
    ItemCompleted { folder: String, item: CompletedItem },
    /// `SyncEngine::syncError(message, category)`.
    SyncError {
        folder: String,
        message: String,
        category: String,
    },
    /// `SyncEngine::newBigFolder(folder, isExternal)`: a new folder larger
    /// than the limit (or an external storage) was found and left undecided
    /// ("Please go in the settings to select it").
    NewBigFolder {
        folder: String,
        path: String,
        external: bool,
    },
    /// `SyncEngine::existingFolderNowBig(folder)`.
    ExistingFolderNowBig { folder: String, path: String },
    /// `SyncEngine::aboutToRemoveAllFiles`: the sync would have deleted all
    /// the files. Upstream asks (`Folder::slotAboutToRemoveAllFiles`); the
    /// engine answered "Restore Files", which loses no data.
    AllFilesDeletedRestored { folder: String },
    /// A Login Flow v2 started with
    /// [`crate::control::Request::StartLogin`] ended: on success the account
    /// was added (or its credentials replaced) and the configuration
    /// reloaded.
    LoginFinished {
        login: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The subscriber missed this many events: ask for the status again.
    Lagged { missed: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ui_event_json_is_tagged() {
        let e = UiEvent::FolderRemoved { folder: "1".into() };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"folder-removed","folder":"1"}"#
        );
        let back: UiEvent = serde_json::from_str(r#"{"event":"lagged","missed":3}"#).unwrap();
        assert_eq!(back, UiEvent::Lagged { missed: 3 });
    }
}
