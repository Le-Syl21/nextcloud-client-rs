// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/lockfilejobs.{h,cpp}` (nextcloud/desktop
// v34.0.5).

//! `LockFileJob`: locks (`LOCK`) or unlocks (`UNLOCK`) a file on the server
//! (the `files_lock` app) and stores the lock state the server answers with
//! in the journal record of the file.
//!
//! The job's two signals become the result of [`LockFileJob::start`]:
//! `finishedWithoutError()` is `Ok(())`, `finishedWithError(httpErrorCode,
//! errorString, lockOwnerName)` is a [`LockFileJobError`].

use std::sync::Arc;

use http::{HeaderMap, HeaderValue};
use nc_dav::jobs::{self, JobOptions, Target};
use nc_dav::{Account, Body, NetworkError, Reply, method};
use nc_journal::journal::{SyncJournalDb, SyncJournalFileRecord};

use crate::filesystem;
use crate::item::{LockOwnerType, LockStatus};
use crate::utility::{qstring_to_int_ok, qstring_to_long_long_ok};

const LOG: &str = "nextcloud.sync.networkjob.lockfile";

/// `LockFileJob::LOCKED_HTTP_ERROR_CODE`.
pub const LOCKED_HTTP_ERROR_CODE: u16 = 423;
/// `LockFileJob::PRECONDITION_FAILED_ERROR_CODE`.
pub const PRECONDITION_FAILED_ERROR_CODE: u16 = 412;

/// `finishedWithError(httpErrorCode, errorString, lockOwnerName)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockFileJobError {
    /// The HTTP status of the reply (0 without one).
    pub http_error_code: u16,
    /// `reply()->errorString()`; empty for a 423.
    pub error_string: String,
    /// For a 423: who holds the lock (the user's display name, or the
    /// editor app for an app or token lock); empty otherwise.
    pub lock_owner_name: String,
}

/// The lock fields decoded from the reply (`_lockStatus`, `_lockOwnerType`,
/// ...), reset before each decoding (`resetState`).
#[derive(Debug, Default)]
struct DecodedLock {
    lock_status: i64,
    /// `static_cast<LockOwnerType>(int)`: kept as the raw value, which is
    /// what the record stores.
    lock_owner_type: i64,
    user_display_name: String,
    editor_name: String,
    user_id: String,
    lock_time: i64,
    lock_timeout: i64,
    lock_token: String,
}

impl DecodedLock {
    fn is_locked(&self) -> bool {
        self.lock_status == LockStatus::LockedItem as i64
    }
}

/// `LockFileJob`.
pub struct LockFileJob {
    account: Arc<Account>,
    journal: Arc<SyncJournalDb>,
    /// `path()`: the server-relative path of the file (`/` + folder
    /// remote path + file).
    path: String,
    requested_lock_state: LockStatus,
    requested_lock_owner_type: LockOwnerType,
    remote_sync_path_with_trailing_slash: String,
    local_sync_path: String,
    existing_etag: String,
    /// `_etag`: the `getetag` of the reply, if any.
    etag: Vec<u8>,
}

impl LockFileJob {
    /// `LockFileJob(account, journal, path, remoteSyncPathWithTrailingSlash,
    /// localSyncPath, etag, requestedLockState, lockOwnerType)`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        account: Arc<Account>,
        journal: Arc<SyncJournalDb>,
        path: &str,
        remote_sync_path_with_trailing_slash: &str,
        local_sync_path: &str,
        etag: &str,
        requested_lock_state: LockStatus,
        lock_owner_type: LockOwnerType,
    ) -> Self {
        let mut local_sync_path = local_sync_path.to_owned();
        if !local_sync_path.ends_with('/') {
            local_sync_path.push('/');
        }
        Self {
            account,
            journal,
            path: path.to_owned(),
            requested_lock_state,
            requested_lock_owner_type: lock_owner_type,
            remote_sync_path_with_trailing_slash: remote_sync_path_with_trailing_slash.to_owned(),
            local_sync_path,
            existing_etag: etag.to_owned(),
            etag: Vec::new(),
        }
    }

    /// `path().mid(_remoteSyncPathWithTrailingSlash.size())`.
    fn relative_path_in_db(&self) -> &str {
        let skip = self
            .remote_sync_path_with_trailing_slash
            .encode_utf16()
            .count();
        let mut units = 0;
        for (i, c) in self.path.char_indices() {
            if units >= skip {
                return &self.path[i..];
            }
            units += c.len_utf16();
        }
        ""
    }

    fn file_record(&self, relative_path_in_db: &str) -> Option<SyncJournalFileRecord> {
        self.journal
            .get_file_record(relative_path_in_db.as_bytes())
            .ok()
            .flatten()
            .filter(SyncJournalFileRecord::is_valid)
    }

    /// `start()` followed by `finished()`.
    pub async fn start(mut self) -> Result<(), LockFileJobError> {
        let mut remote_path = self.path.clone();

        let relative_path_in_db = self.relative_path_in_db().to_owned();
        if let Some(record) = self.file_record(&relative_path_in_db)
            && record.is_e2e_encrypted()
        {
            remote_path = format!(
                "{}{}",
                self.remote_sync_path_with_trailing_slash,
                String::from_utf8_lossy(&record.e2e_mangled_name)
            );
            log::debug!(target: LOG, "will (un)lock e2ee file path={} remotePath={remote_path}", self.path);
        }

        log::info!(target: LOG, "start with path: {remote_path} lock state: {:?} lock owner type: {:?}", self.requested_lock_state, self.requested_lock_owner_type);

        let mut headers = HeaderMap::new();
        headers.insert("X-User-Lock", HeaderValue::from_static("1"));
        if self.account.capabilities().files_lock_type_available() {
            match self.requested_lock_owner_type {
                LockOwnerType::UserLock => {
                    headers.insert("X-User-Lock-Type", HeaderValue::from_static("0"));
                }
                LockOwnerType::TokenLock => {
                    headers.insert("X-User-Lock-Type", HeaderValue::from_static("2"));
                }
                LockOwnerType::AppLock => {}
            }
        }

        let verb = match self.requested_lock_state {
            LockStatus::LockedItem => {
                // `QString::toLatin1()`: characters outside Latin-1 become '?'.
                let latin1: Vec<u8> = self
                    .existing_etag
                    .chars()
                    .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                    .collect();
                let mut value = Vec::with_capacity(latin1.len() + 2);
                value.push(b'"');
                value.extend_from_slice(&latin1);
                value.push(b'"');
                if let Ok(v) = HeaderValue::from_bytes(&value) {
                    headers.insert(http::header::IF_MATCH, v);
                }
                method::lock()
            }
            LockStatus::UnlockedItem => method::unlock(),
        };

        let reply = jobs::send(
            &self.account,
            verb,
            &Target::Dav(remote_path),
            headers,
            Body::empty(),
            &JobOptions::default(),
        )
        .await;
        self.finished(&reply)
    }

    /// `finished()`.
    fn finished(&mut self, reply: &Reply) -> Result<(), LockFileJobError> {
        if reply.error != NetworkError::NoError {
            log::info!(target: LOG, "finished with error {:?} {} {:?} {:?} {}", reply.error, reply.error_string(), self.requested_lock_state, self.requested_lock_owner_type, self.existing_etag);
            let http_error_code = reply.http_status;
            if http_error_code == LOCKED_HTTP_ERROR_CODE {
                let record = self.handle_reply(reply);
                let lock_owner_name =
                    if record.lockstate.lock_owner_type == LockOwnerType::UserLock as i64 {
                        record.lockstate.lock_owner_display_name
                    } else {
                        record.lockstate.lock_editor_app
                    };
                Err(LockFileJobError {
                    http_error_code,
                    error_string: String::new(),
                    lock_owner_name,
                })
            } else if http_error_code == PRECONDITION_FAILED_ERROR_CODE {
                let record = self.handle_reply(reply);
                if self.requested_lock_state == LockStatus::UnlockedItem && !record.lockstate.locked
                {
                    Ok(())
                } else {
                    Err(LockFileJobError {
                        http_error_code,
                        error_string: reply.error_string(),
                        lock_owner_name: String::new(),
                    })
                }
            } else {
                Err(LockFileJobError {
                    http_error_code,
                    error_string: reply.error_string(),
                    lock_owner_name: String::new(),
                })
            }
        } else {
            log::info!(target: LOG, "success {} {:?} {:?}", self.path, self.requested_lock_state, self.requested_lock_owner_type);
            self.handle_reply(reply);
            Ok(())
        }
    }

    /// `setFileRecordLocked(record)`.
    fn set_file_record_locked(&self, lock: &DecodedLock, record: &mut SyncJournalFileRecord) {
        record.lockstate.locked = lock.is_locked();
        record.lockstate.lock_owner_type = lock.lock_owner_type;
        record.lockstate.lock_owner_display_name = lock.user_display_name.clone();
        record.lockstate.lock_owner_id = lock.user_id.clone();
        record.lockstate.lock_editor_app = lock.editor_name.clone();
        record.lockstate.lock_time = lock.lock_time;
        record.lockstate.lock_timeout = lock.lock_timeout;
        record.lockstate.lock_token = lock.lock_token.clone();
        if !self.etag.is_empty() {
            record.etag = self.etag.clone();
        }
    }

    /// `handleReply()`: decodes the lock properties of the reply and, when
    /// they are complete, stores them in the file's journal record (making
    /// the local file read-only when someone else holds the lock). Returns
    /// that record, or an empty one.
    fn handle_reply(&mut self, reply: &Reply) -> SyncJournalFileRecord {
        let mut lock = DecodedLock::default();
        let elements = nc_dav::xml::read_element_texts(
            &reply.body,
            &[
                "lock",
                "lock-owner-type",
                "lock-owner-displayname",
                "lock-owner",
                "lock-time",
                "lock-timeout",
                "lock-owner-editor",
                "getetag",
                "lock-token",
            ],
        );
        for (name, value_text) in elements {
            self.decode_start_element(&mut lock, &name, value_text);
        }

        if lock.is_locked() {
            if lock.lock_owner_type == LockOwnerType::UserLock as i64
                && lock.user_display_name.is_empty()
            {
                return SyncJournalFileRecord::default();
            }
            if lock.lock_owner_type == LockOwnerType::AppLock as i64 && lock.editor_name.is_empty()
            {
                return SyncJournalFileRecord::default();
            }
            if lock.user_id.is_empty() {
                return SyncJournalFileRecord::default();
            }
            if lock.lock_time <= 0 {
                return SyncJournalFileRecord::default();
            }
        }

        let relative_path_in_db = self.relative_path_in_db().to_owned();
        let Some(mut record) = self.file_record(&relative_path_in_db) else {
            return SyncJournalFileRecord::default();
        };
        self.set_file_record_locked(&lock, &mut record);
        if lock.is_locked()
            && (lock.lock_owner_type == LockOwnerType::AppLock as i64
                || lock.user_id != self.account.dav_user())
        {
            filesystem::set_file_read_only(
                &format!("{}{relative_path_in_db}", self.local_sync_path),
                true,
            );
        }
        if let Err(e) = self.journal.set_file_record(&record) {
            log::warn!(target: LOG, "Error when setting the file record to the database {} {e}", record.path_str());
        }
        self.journal.commit("lock file job", true);
        record
    }

    /// `decodeStartElement(name, reader)`.
    fn decode_start_element(&mut self, lock: &mut DecodedLock, name: &str, value_text: String) {
        match name {
            "lock" => {
                if !value_text.is_empty()
                    && let Some(converted_value) = qstring_to_int_ok(&value_text)
                {
                    lock.lock_status = converted_value;
                }
            }
            "lock-owner-type" => {
                lock.lock_owner_type =
                    qstring_to_int_ok(&value_text).unwrap_or(LockOwnerType::UserLock as i64);
            }
            "lock-owner-displayname" => lock.user_display_name = value_text,
            "lock-owner" => lock.user_id = value_text,
            "lock-time" => {
                if let Some(v) = qstring_to_long_long_ok(&value_text) {
                    lock.lock_time = v;
                }
            }
            "lock-timeout" => {
                if let Some(v) = qstring_to_long_long_ok(&value_text) {
                    lock.lock_timeout = v;
                }
            }
            "lock-owner-editor" => lock.editor_name = value_text,
            "getetag" => self.etag = value_text.into_bytes(),
            "lock-token" => lock.lock_token = value_text,
            _ => {}
        }
    }
}
