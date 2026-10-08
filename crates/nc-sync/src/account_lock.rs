// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the file locking methods of upstream `src/libsync/account.cpp`
// (`setLockFileState`, `removeLockStatusChangeInprogress`, `fileLockStatus`,
// `fileCanBeUnlocked`) (nextcloud/desktop v34.0.5).

//! File locking on behalf of an account.
//!
//! They are methods of `Account` upstream; they live here because they
//! need the journal and the [`LockFileJob`], which `nc-dav` does not know.
//! The account's `lockFileSuccess()` / `lockFileError(message)` signals are
//! the result of the future [`set_lock_file_state`] returns.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nc_dav::Account;
use nc_journal::journal::SyncJournalDb;

use crate::item::{LockOwnerType, LockStatus};
use crate::lock_file_jobs::{LOCKED_HTTP_ERROR_CODE, LockFileJob};

const LOG: &str = "nextcloud.sync.account";

/// The running `setLockFileState` request: resolves to `Ok(())` for
/// `lockFileSuccess()` and to `Err(message)` for `lockFileError(message)`.
pub type LockFileStateFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Removes the in-progress entry when the request ends (or is dropped).
struct InProgressGuard {
    account: Arc<Account>,
    server_relative_path: String,
    lock_status: LockStatus,
}

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        self.account.remove_lock_status_change_in_progress(
            &self.server_relative_path,
            self.lock_status as i64,
        );
    }
}

/// `Account::setLockFileState(serverRelativePath,
/// remoteSyncPathWithTrailingSlash, localSyncPath, etag, journal,
/// lockStatus, lockOwnerType)`.
///
/// Returns `None`, without starting anything, when a request for the same
/// file and state is already running (upstream returns without emitting a
/// signal). The request is registered as running at once, like upstream
/// starts its job synchronously; the returned future runs the job.
#[allow(clippy::too_many_arguments)]
pub fn set_lock_file_state(
    account: &Arc<Account>,
    server_relative_path: &str,
    remote_sync_path_with_trailing_slash: &str,
    local_sync_path: &str,
    etag: &str,
    journal: &Arc<SyncJournalDb>,
    lock_status: LockStatus,
    lock_owner_type: LockOwnerType,
) -> Option<LockFileStateFuture> {
    if !account.add_lock_status_change_in_progress(server_relative_path, lock_status as i64) {
        log::warn!(target: LOG, "Already running a job with lockStatus: {lock_status:?}  for:  {server_relative_path}");
        return None;
    }
    let guard = InProgressGuard {
        account: account.clone(),
        server_relative_path: server_relative_path.to_owned(),
        lock_status,
    };
    let job = LockFileJob::new(
        account.clone(),
        journal.clone(),
        server_relative_path,
        remote_sync_path_with_trailing_slash,
        local_sync_path,
        etag,
        lock_status,
        lock_owner_type,
    );
    let server_relative_path = server_relative_path.to_owned();
    Some(Box::pin(async move {
        let result = job.start().await;
        drop(guard);
        result.map_err(|e| {
            // `serverRelativePath.mid(1)`
            let file_path = server_relative_path
                .char_indices()
                .nth(1)
                .map_or("", |(i, _)| &server_relative_path[i..]);
            if e.http_error_code == LOCKED_HTTP_ERROR_CODE {
                format!(
                    "File {file_path} is already locked by {}.",
                    e.lock_owner_name
                )
            } else if lock_status == LockStatus::LockedItem {
                format!(
                    "Lock operation on {file_path} failed with error {}",
                    e.error_string
                )
            } else {
                format!(
                    "Unlock operation on {file_path} failed with error {}",
                    e.error_string
                )
            }
        })
    }))
}

/// `Account::fileLockStatus(journal, folderRelativePath)`.
pub fn file_lock_status(journal: &SyncJournalDb, folder_relative_path: &str) -> LockStatus {
    match journal.get_file_record(folder_relative_path.as_bytes()) {
        Ok(Some(record)) if record.lockstate.locked => LockStatus::LockedItem,
        _ => LockStatus::UnlockedItem,
    }
}

/// `Account::fileCanBeUnlocked(journal, folderRelativePath)`.
pub fn file_can_be_unlocked(
    account: &Account,
    journal: &SyncJournalDb,
    folder_relative_path: &str,
) -> bool {
    // `getFileRecord` succeeds (with an invalid record) for a file that is
    // not in the journal.
    let Ok(record) = journal.get_file_record(folder_relative_path.as_bytes()) else {
        return false;
    };
    let lockstate = record.map(|r| r.lockstate).unwrap_or_default();
    if lockstate.lock_owner_type == LockOwnerType::AppLock as i64 {
        log::warn!(target: LOG, "{folder_relative_path} cannot be unlocked: app lock");
        return false;
    }
    if lockstate.lock_owner_type == LockOwnerType::UserLock as i64
        && lockstate.lock_owner_id != account.dav_user()
    {
        log::warn!(target: LOG, "{folder_relative_path} cannot be unlocked: user lock from {}", lockstate.lock_owner_id);
        return false;
    }
    if lockstate.lock_owner_type == LockOwnerType::TokenLock as i64
        && lockstate.lock_token.is_empty()
    {
        log::warn!(target: LOG, "{folder_relative_path} cannot be unlocked: token lock without known token");
        return false;
    }
    true
}
