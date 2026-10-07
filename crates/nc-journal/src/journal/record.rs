// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Port of upstream `src/common/syncjournalfilerecord.{h,cpp}` (nextcloud/desktop v34.0.5).

//! Rows of the journal tables.
//!
//! Field types mirror upstream: `QByteArray` becomes `Vec<u8>`, `QString`
//! becomes `String`.

use crate::csync::{ItemType, JournalDbEncryptionStatus};
use crate::remote_permissions::RemotePermissions;

/// Lock state of a file (`SyncJournalFileLockInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncJournalFileLockInfo {
    pub locked: bool,
    pub lock_owner_display_name: String,
    pub lock_owner_id: String,
    pub lock_owner_type: i64,
    pub lock_editor_app: String,
    pub lock_time: i64,
    pub lock_timeout: i64,
    pub lock_token: String,
}

/// Folder quota (`SyncJournalFileRecord::FolderQuota`); `-1` means unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderQuota {
    pub bytes_used: i64,
    pub bytes_available: i64,
}

impl Default for FolderQuota {
    fn default() -> Self {
        Self {
            bytes_used: -1,
            bytes_available: -1,
        }
    }
}

/// One row of the `metadata` table (`SyncJournalFileRecord`).
#[derive(Clone, Debug, Default)]
pub struct SyncJournalFileRecord {
    pub path: Vec<u8>,
    pub inode: u64,
    pub modtime: i64,
    pub item_type: ItemType,
    /// Stored in the `md5` column for historical reasons.
    pub etag: Vec<u8>,
    pub file_id: Vec<u8>,
    pub file_size: i64,
    pub remote_perm: RemotePermissions,
    pub server_has_ignored_files: bool,
    /// `"TYPE:checksum"`, e.g. `"SHA1:baff"`.
    pub checksum_header: Vec<u8>,
    pub e2e_mangled_name: Vec<u8>,
    pub e2e_encryption_status: JournalDbEncryptionStatus,
    pub lockstate: SyncJournalFileLockInfo,
    pub is_shared: bool,
    pub last_share_state_fetched_timestamp: i64,
    pub shared_by_me: bool,
    pub is_live_photo: bool,
    pub live_photo_file: String,
    pub folder_quota: FolderQuota,
}

impl SyncJournalFileRecord {
    /// A record is valid when it has a path (found in the journal).
    pub fn is_valid(&self) -> bool {
        !self.path.is_empty()
    }

    /// Returns the numeric part of the full id in `file_id` (the server's
    /// internal file id, used in private links): the id up until the first
    /// non-numeric character.
    pub fn numeric_file_id(&self) -> &[u8] {
        let end = self
            .file_id
            .iter()
            .position(|c| !c.is_ascii_digit())
            .unwrap_or(self.file_id.len());
        &self.file_id[..end]
    }

    pub fn is_directory(&self) -> bool {
        matches!(
            self.item_type,
            ItemType::VirtualDirectory | ItemType::Directory
        )
    }

    pub fn is_file(&self) -> bool {
        matches!(
            self.item_type,
            ItemType::File | ItemType::VirtualFileDehydration
        )
    }

    pub fn is_virtual_file(&self) -> bool {
        matches!(
            self.item_type,
            ItemType::VirtualFile | ItemType::VirtualFileDownload
        )
    }

    /// The path as a string (`QString::fromUtf8`, lossy).
    pub fn path_str(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }

    pub fn is_e2e_encrypted(&self) -> bool {
        self.e2e_encryption_status != JournalDbEncryptionStatus::NotEncrypted
    }
}

/// Upstream `operator==` only compares these fields.
impl PartialEq for SyncJournalFileRecord {
    fn eq(&self, rhs: &Self) -> bool {
        self.path == rhs.path
            && self.inode == rhs.inode
            && self.modtime == rhs.modtime
            && self.item_type == rhs.item_type
            && self.etag == rhs.etag
            && self.file_id == rhs.file_id
            && self.file_size == rhs.file_size
            && self.remote_perm == rhs.remote_perm
            && self.server_has_ignored_files == rhs.server_has_ignored_files
            && self.checksum_header == rhs.checksum_header
    }
}

/// Category of an error blacklist entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ErrorCategory {
    /// Normal errors have no special behavior.
    #[default]
    Normal = 0,
    /// These get a special summary message.
    InsufficientRemoteStorage = 1,
}

impl ErrorCategory {
    pub fn from_db(value: i64) -> Self {
        if value == 1 {
            Self::InsufficientRemoteStorage
        } else {
            Self::Normal
        }
    }
}

/// One row of the `blacklist` table (`SyncJournalErrorBlacklistRecord`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncJournalErrorBlacklistRecord {
    /// The number of times the operation was unsuccessful so far.
    pub retry_count: i32,
    /// The last error string.
    pub error_string: String,
    /// The error category. Sometimes used for special actions.
    pub error_category: ErrorCategory,
    pub last_try_modtime: i64,
    pub last_try_etag: Vec<u8>,
    /// The last time the operation was attempted (in s since epoch).
    pub last_try_time: i64,
    /// The number of seconds the file shall be ignored.
    pub ignore_duration: i64,
    pub file: String,
    pub rename_target: String,
    /// The last X-Request-ID of the request that failed.
    pub request_id: Vec<u8>,
}

impl SyncJournalErrorBlacklistRecord {
    pub fn is_valid(&self) -> bool {
        !self.file.is_empty()
            && (!self.last_try_etag.is_empty() || self.last_try_modtime != 0)
            && self.last_try_time > 0
    }
}

/// One row of the `conflicts` (or `caseconflicts`) table (`ConflictRecord`).
///
/// The "conflict file" is the file that has the conflict tag in the filename,
/// and the base file is the file that it's a conflict for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRecord {
    /// Sync-folder relative path to the file with the conflict tag in the name.
    pub path: Vec<u8>,
    /// File id of the base file.
    pub base_file_id: Vec<u8>,
    /// Modtime of the base file; may not be available and be -1.
    pub base_modtime: i64,
    /// Etag of the base file; may not be available and empty.
    pub base_etag: Vec<u8>,
    /// The path of the original file at the time the conflict was created.
    /// Prefer querying by `base_file_id` to get the *current* base path.
    pub initial_base_path: Vec<u8>,
}

impl Default for ConflictRecord {
    fn default() -> Self {
        Self {
            path: Vec::new(),
            base_file_id: Vec::new(),
            base_modtime: -1,
            base_etag: Vec::new(),
            initial_base_path: Vec::new(),
        }
    }
}

impl ConflictRecord {
    pub fn is_valid(&self) -> bool {
        !self.path.is_empty()
    }
}
