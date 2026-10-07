// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/syncfileitem.{h,cpp}`, the `SyncInstructions`
// enum of `src/csync/csync.h` and `src/common/syncitemenums.h`
// (nextcloud/desktop v34.0.5).

//! `SyncFileItem`: one entry of the sync, from discovery to propagation.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use nc_journal::csync::{ItemEncryptionStatus, ItemType, JournalDbEncryptionStatus};
use nc_journal::journal::{FolderQuota, SyncJournalFileLockInfo, SyncJournalFileRecord};
use nc_journal::remote_permissions::{MountedPermissionAlgorithm, Permission, RemotePermissions};

/// `SyncInstructions` (csync.h). The values are upstream's bit flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Instruction {
    /// Nothing to do (UPDATE|RECONCILE)
    #[default]
    None = 0,
    /// There was changed compared to the DB (UPDATE)
    Eval = 1 << 0,
    /// The file need to be removed (RECONCILE)
    Remove = 1 << 1,
    /// The file need to be renamed (RECONCILE)
    Rename = 1 << 2,
    /// The file is new, it is the destination of a rename (UPDATE)
    EvalRename = 1 << 11,
    /// The file is new compared to the db (UPDATE)
    New = 1 << 3,
    /// The file need to be downloaded because it is a conflict (RECONCILE)
    Conflict = 1 << 4,
    /// The file is ignored (UPDATE|RECONCILE)
    Ignore = 1 << 5,
    /// The file need to be pushed to the other remote (RECONCILE)
    Sync = 1 << 6,
    StatError = 1 << 7,
    Error = 1 << 8,
    /// Like NEW, but deletes the old entity first (RECONCILE). Used when the
    /// type of something changes from directory to file or back.
    TypeChange = 1 << 9,
    /// If the etag has been updated and need to be writen to the db, but
    /// without any propagation (UPDATE|RECONCILE)
    UpdateMetadata = 1 << 10,
    /// The file need to be downloaded because it is a case clash conflict (RECONCILE)
    CaseClashConflict = 1 << 12,
    /// vfs item metadata are out of sync and we need to tell operating system about it
    UpdateVfsMetadata = 1 << 13,
    /// encryption metadata needs update after certificate was migrated
    UpdateEncryptionMetadata = 1 << 14,
}

impl Instruction {
    /// The upstream enumerator name (as logged by `Q_ENUM`).
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "CSYNC_INSTRUCTION_NONE",
            Self::Eval => "CSYNC_INSTRUCTION_EVAL",
            Self::Remove => "CSYNC_INSTRUCTION_REMOVE",
            Self::Rename => "CSYNC_INSTRUCTION_RENAME",
            Self::EvalRename => "CSYNC_INSTRUCTION_EVAL_RENAME",
            Self::New => "CSYNC_INSTRUCTION_NEW",
            Self::Conflict => "CSYNC_INSTRUCTION_CONFLICT",
            Self::Ignore => "CSYNC_INSTRUCTION_IGNORE",
            Self::Sync => "CSYNC_INSTRUCTION_SYNC",
            Self::StatError => "CSYNC_INSTRUCTION_STAT_ERROR",
            Self::Error => "CSYNC_INSTRUCTION_ERROR",
            Self::TypeChange => "CSYNC_INSTRUCTION_TYPE_CHANGE",
            Self::UpdateMetadata => "CSYNC_INSTRUCTION_UPDATE_METADATA",
            Self::CaseClashConflict => "CSYNC_INSTRUCTION_CASE_CLASH_CONFLICT",
            Self::UpdateVfsMetadata => "CSYNC_INSTRUCTION_UPDATE_VFS_METADATA",
            Self::UpdateEncryptionMetadata => "CSYNC_INSTRUCTION_UPDATE_ENCRYPTION_METADATA",
        }
    }
}

/// `SyncFileItem::Direction`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Direction {
    #[default]
    None = 0,
    Up,
    Down,
}

/// `SyncFileItem::Status`. The order is upstream's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    #[default]
    NoStatus,
    /// Error that causes the sync to stop
    FatalError,
    /// Error attached to a particular file
    NormalError,
    /// More like an information
    SoftError,
    /// Marks a conflict, old or new.
    Conflict,
    /// The file is in the ignored list (or blacklisted with no retries left)
    FileIgnored,
    /// The file is locked
    FileLocked,
    /// The file was restored because what should have been done was not allowed
    Restoration,
    /// The filename is invalid on this platform and could not created.
    FileNameInvalid,
    /// The filename contains invalid characters and can not be uploaded to the server
    FileNameInvalidOnServer,
    /// There is a file name clash (e.g. attempting to download test.txt when
    /// TEST.TXT already exists on a case-insensitive file system)
    FileNameClash,
    /// For errors that should only appear in the error view.
    DetailError,
    /// For files whose errors were blacklisted.
    BlacklistedError,
    /// The file was properly synced
    Success,
}

/// `SyncFileItemEnums::LockStatus`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LockStatus {
    #[default]
    UnlockedItem = 0,
    LockedItem = 1,
}

/// `SyncFileItemEnums::LockOwnerType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LockOwnerType {
    #[default]
    UserLock = 0,
    AppLock = 1,
    TokenLock = 2,
}

impl LockOwnerType {
    /// `static_cast<LockOwnerType>(int)`; unknown values map to `UserLock`.
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::AppLock,
            2 => Self::TokenLock,
            _ => Self::UserLock,
        }
    }
}

/// `SyncFileItem::SynchronizationOptions`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SynchronizationOptions {
    #[default]
    NormalSynchronization,
    WantsPermanentDeletion,
    MoveToClientTrashBin,
}

/// `ErrorCategory` (progressdispatcher.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    #[default]
    NoError,
    GenericError,
    NetworkError,
    InsufficientRemoteStorage,
}

/// `EncryptionStatusEnums::fromDbEncryptionStatus`.
pub fn from_db_encryption_status(s: JournalDbEncryptionStatus) -> ItemEncryptionStatus {
    match s {
        JournalDbEncryptionStatus::Encrypted => ItemEncryptionStatus::Encrypted,
        JournalDbEncryptionStatus::EncryptedMigratedV1_2 => {
            ItemEncryptionStatus::EncryptedMigratedV1_2
        }
        JournalDbEncryptionStatus::EncryptedMigratedV1_2Invalid => ItemEncryptionStatus::Encrypted,
        JournalDbEncryptionStatus::EncryptedMigratedV2_0 => {
            ItemEncryptionStatus::EncryptedMigratedV2_0
        }
        JournalDbEncryptionStatus::NotEncrypted => ItemEncryptionStatus::NotEncrypted,
    }
}

/// `EncryptionStatusEnums::toDbEncryptionStatus`.
pub fn to_db_encryption_status(s: ItemEncryptionStatus) -> JournalDbEncryptionStatus {
    match s {
        ItemEncryptionStatus::Encrypted => JournalDbEncryptionStatus::Encrypted,
        ItemEncryptionStatus::EncryptedMigratedV1_2 => {
            JournalDbEncryptionStatus::EncryptedMigratedV1_2
        }
        ItemEncryptionStatus::EncryptedMigratedV2_0 => {
            JournalDbEncryptionStatus::EncryptedMigratedV2_0
        }
        ItemEncryptionStatus::NotEncrypted => JournalDbEncryptionStatus::NotEncrypted,
    }
}

/// `EncryptionStatusEnums::fromEndToEndEncryptionApiVersion`.
pub fn from_end_to_end_encryption_api_version(version: f64) -> ItemEncryptionStatus {
    if version >= 2.0 {
        ItemEncryptionStatus::EncryptedMigratedV2_0
    } else if version >= 1.2 {
        ItemEncryptionStatus::EncryptedMigratedV1_2
    } else if version >= 1.0 {
        ItemEncryptionStatus::Encrypted
    } else {
        ItemEncryptionStatus::NotEncrypted
    }
}

/// `SyncFileItem::FolderQuota` (`-1` means unknown).
pub type ItemFolderQuota = FolderQuota;

/// One item of the sync (`SyncFileItem`).
#[derive(Clone, Debug)]
pub struct SyncFileItem {
    /// The syncfolder-relative filesystem path that the operation is about.
    /// For rename operation this is the rename source and the target is in
    /// `rename_target`.
    pub file: String,
    /// For renames: the name `file` should be renamed to; otherwise empty.
    /// Use [`SyncFileItem::destination`] to find the sync target.
    pub rename_target: String,
    /// The db-path of this item.
    pub original_file: String,
    /// The encrypted name on the server (E2EE, unused by this port).
    pub encrypted_file_name: String,
    pub item_type: ItemType,
    pub direction: Direction,
    pub server_has_ignored_files: bool,
    /// Whether there's an entry in the blacklist table.
    pub has_blacklist_entry: bool,
    /// If true and NormalError, this error may be blacklisted.
    pub error_may_be_blacklisted: bool,
    pub status: Status,
    /// The original operation was forbidden, and this is a restoration.
    pub is_restoration: bool,
    /// The file is removed or ignored because it is in the selective sync list.
    pub is_selective_sync: bool,
    pub e2e_encryption_status: ItemEncryptionStatus,
    pub e2e_encryption_server_capability: ItemEncryptionStatus,
    pub e2e_encryption_status_remote: ItemEncryptionStatus,
    pub http_error_code: u16,
    pub remote_perm: RemotePermissions,
    /// Contains a string only in case of error.
    pub error_string: String,
    pub error_exception_name: String,
    pub error_exception_message: String,
    pub response_time_stamp: Vec<u8>,
    /// X-Request-Id of the failed request.
    pub request_id: Vec<u8>,
    /// The number of affected items by the operation on this item.
    pub affected_items: u32,
    pub instruction: Instruction,
    pub modtime: i64,
    pub etag: Vec<u8>,
    pub size: i64,
    pub inode: u64,
    pub file_id: Vec<u8>,
    /// The checksum header of the "new" side, matching `size` and `modtime`.
    pub checksum_header: Vec<u8>,
    /// The size and modtime of the file getting overwritten.
    pub previous_size: i64,
    pub previous_modtime: i64,
    pub direct_download_url: String,
    pub direct_download_cookies: String,
    pub locked: LockStatus,
    pub lock_owner_id: String,
    pub lock_owner_display_name: String,
    pub lock_owner_type: LockOwnerType,
    pub lock_editor_app: String,
    pub lock_time: i64,
    pub lock_timeout: i64,
    pub lock_token: String,
    pub is_shared: bool,
    pub last_share_state_fetched_timestamp: i64,
    pub shared_by_me: bool,
    pub is_file_drop_detected: bool,
    pub is_encrypted_metadata_need_update: bool,
    pub is_any_invalid_char_child: bool,
    pub is_any_case_clash_child: bool,
    pub is_live_photo: bool,
    pub live_photo_file: String,
    pub is_permissions_invalid: bool,
    pub discovery_result: String,
    pub wants_specific_actions: SynchronizationOptions,
    pub folder_quota: ItemFolderQuota,
}

impl Default for SyncFileItem {
    fn default() -> Self {
        Self {
            file: String::new(),
            rename_target: String::new(),
            original_file: String::new(),
            encrypted_file_name: String::new(),
            item_type: ItemType::Skip,
            direction: Direction::None,
            server_has_ignored_files: false,
            has_blacklist_entry: false,
            error_may_be_blacklisted: false,
            status: Status::NoStatus,
            is_restoration: false,
            is_selective_sync: false,
            e2e_encryption_status: ItemEncryptionStatus::NotEncrypted,
            e2e_encryption_server_capability: ItemEncryptionStatus::NotEncrypted,
            e2e_encryption_status_remote: ItemEncryptionStatus::NotEncrypted,
            http_error_code: 0,
            remote_perm: RemotePermissions::null(),
            error_string: String::new(),
            error_exception_name: String::new(),
            error_exception_message: String::new(),
            response_time_stamp: Vec::new(),
            request_id: Vec::new(),
            affected_items: 1,
            instruction: Instruction::None,
            modtime: 0,
            etag: Vec::new(),
            size: 0,
            inode: 0,
            file_id: Vec::new(),
            checksum_header: Vec::new(),
            previous_size: 0,
            previous_modtime: 0,
            direct_download_url: String::new(),
            direct_download_cookies: String::new(),
            locked: LockStatus::UnlockedItem,
            lock_owner_id: String::new(),
            lock_owner_display_name: String::new(),
            lock_owner_type: LockOwnerType::UserLock,
            lock_editor_app: String::new(),
            lock_time: 0,
            lock_timeout: 0,
            lock_token: String::new(),
            is_shared: false,
            last_share_state_fetched_timestamp: 0,
            shared_by_me: false,
            is_file_drop_detected: false,
            is_encrypted_metadata_need_update: false,
            is_any_invalid_char_child: false,
            is_any_case_clash_child: false,
            is_live_photo: false,
            live_photo_file: String::new(),
            is_permissions_invalid: false,
            discovery_result: String::new(),
            wants_specific_actions: SynchronizationOptions::NormalSynchronization,
            folder_quota: ItemFolderQuota::default(),
        }
    }
}

/// `SyncFileItemPtr` (`QSharedPointer<SyncFileItem>`): items are shared
/// between the discovery, the sorted item list and the propagator jobs, and
/// mutated by all of them. The engine is single-threaded.
pub type SyncFileItemPtr = Rc<RefCell<SyncFileItem>>;

/// `SyncFileItemVector`.
pub type SyncFileItemVector = Vec<SyncFileItemPtr>;

/// `SyncFileItemPtr::create()`.
pub fn new_item() -> SyncFileItemPtr {
    Rc::new(RefCell::new(SyncFileItem::default()))
}

/// Current time in ms since the epoch (`QDateTime::currentMSecsSinceEpoch`).
pub fn current_msecs_since_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl SyncFileItem {
    /// `destination()`: the rename target if set, else `file`.
    pub fn destination(&self) -> &str {
        if !self.rename_target.is_empty() {
            &self.rename_target
        } else {
            &self.file
        }
    }

    pub fn is_empty(&self) -> bool {
        self.file.is_empty()
    }

    pub fn is_directory(&self) -> bool {
        matches!(
            self.item_type,
            ItemType::Directory | ItemType::VirtualDirectory
        )
    }

    /// True if the item had any kind of error.
    pub fn has_error_status(&self) -> bool {
        matches!(
            self.status,
            Status::SoftError | Status::NormalError | Status::FatalError
        ) || !self.error_string.is_empty()
    }

    /// Whether this item should appear on the issues tab.
    pub fn show_in_issues_tab(&self) -> bool {
        self.has_error_status() || self.status == Status::Conflict
    }

    /// Whether this item should appear on the protocol tab.
    pub fn show_in_protocol_tab(&self) -> bool {
        (!self.show_in_issues_tab() || self.status == Status::Restoration)
            // Don't show conflicts that were resolved as "not a conflict after all"
            && !(self.instruction == Instruction::Conflict && self.status == Status::Success)
    }

    pub fn is_encrypted(&self) -> bool {
        self.e2e_encryption_status != ItemEncryptionStatus::NotEncrypted
    }

    /// `toSyncJournalFileRecordWithInode(localFileName)`.
    pub fn to_sync_journal_file_record_with_inode(
        &self,
        local_file_name: &str,
    ) -> SyncJournalFileRecord {
        let mut item_type = self.item_type;
        // Some types should never be written to the database when propagation completes
        if item_type == ItemType::VirtualFileDownload {
            item_type = ItemType::File;
        }
        if item_type == ItemType::VirtualFileDehydration {
            item_type = ItemType::VirtualFile;
        }
        let mut inode = self.inode;
        match crate::filesystem::get_inode(local_file_name) {
            Some(i) => inode = i,
            None => {
                // use the "old" inode coming with the item for the case where the
                // filesystem stat fails. That can happen if the the file was removed
                // or renamed meanwhile. For the rename case we still need the inode to
                // detect the rename though.
                log::warn!(target: "nextcloud.sync.fileitem", "Failed to query the 'inode' for file {local_file_name}");
            }
        }
        SyncJournalFileRecord {
            path: self.destination().as_bytes().to_vec(),
            inode,
            modtime: self.modtime,
            item_type,
            etag: self.etag.clone(),
            file_id: self.file_id.clone(),
            file_size: self.size,
            remote_perm: self.remote_perm,
            server_has_ignored_files: self.server_has_ignored_files,
            checksum_header: self.checksum_header.clone(),
            e2e_mangled_name: self.encrypted_file_name.as_bytes().to_vec(),
            e2e_encryption_status: to_db_encryption_status(self.e2e_encryption_status),
            lockstate: SyncJournalFileLockInfo {
                locked: self.locked == LockStatus::LockedItem,
                lock_owner_display_name: self.lock_owner_display_name.clone(),
                lock_owner_id: self.lock_owner_id.clone(),
                lock_owner_type: self.lock_owner_type as i64,
                lock_editor_app: self.lock_editor_app.clone(),
                lock_time: self.lock_time,
                lock_timeout: self.lock_timeout,
                lock_token: self.lock_token.clone(),
            },
            is_shared: self.is_shared,
            last_share_state_fetched_timestamp: self.last_share_state_fetched_timestamp,
            shared_by_me: self.shared_by_me,
            is_live_photo: self.is_live_photo,
            live_photo_file: self.live_photo_file.clone(),
            folder_quota: FolderQuota {
                bytes_used: self.folder_quota.bytes_used,
                bytes_available: self.folder_quota.bytes_available,
            },
        }
    }

    /// `fromSyncJournalFileRecord(rec)`.
    pub fn from_sync_journal_file_record(rec: &SyncJournalFileRecord) -> Self {
        let e2e = from_db_encryption_status(rec.e2e_encryption_status);
        Self {
            file: rec.path_str(),
            inode: rec.inode,
            modtime: rec.modtime,
            item_type: rec.item_type,
            etag: rec.etag.clone(),
            file_id: rec.file_id.clone(),
            size: rec.file_size,
            remote_perm: rec.remote_perm,
            server_has_ignored_files: rec.server_has_ignored_files,
            checksum_header: rec.checksum_header.clone(),
            encrypted_file_name: String::from_utf8_lossy(&rec.e2e_mangled_name).into_owned(),
            e2e_encryption_status: e2e,
            e2e_encryption_server_capability: e2e,
            locked: if rec.lockstate.locked {
                LockStatus::LockedItem
            } else {
                LockStatus::UnlockedItem
            },
            lock_owner_display_name: rec.lockstate.lock_owner_display_name.clone(),
            lock_owner_id: rec.lockstate.lock_owner_id.clone(),
            lock_owner_type: LockOwnerType::from_i64(rec.lockstate.lock_owner_type),
            lock_editor_app: rec.lockstate.lock_editor_app.clone(),
            lock_time: rec.lockstate.lock_time,
            lock_timeout: rec.lockstate.lock_timeout,
            lock_token: rec.lockstate.lock_token.clone(),
            shared_by_me: rec.shared_by_me,
            is_shared: rec.is_shared,
            last_share_state_fetched_timestamp: rec.last_share_state_fetched_timestamp,
            is_live_photo: rec.is_live_photo,
            live_photo_file: rec.live_photo_file.clone(),
            folder_quota: rec.folder_quota,
            ..Self::default()
        }
    }

    /// `fromProperties(filePath, properties, algorithm)`.
    pub fn from_properties(
        file_path: &str,
        properties: &std::collections::BTreeMap<String, String>,
        algorithm: MountedPermissionAlgorithm,
    ) -> Self {
        let is_directory = properties
            .get("resourcetype")
            .is_some_and(|v| v.contains("collection"));
        let mut item = Self {
            file: file_path.to_owned(),
            original_file: file_path.to_owned(),
            item_type: if is_directory {
                ItemType::Directory
            } else {
                ItemType::File
            },
            size: if is_directory {
                0
            } else {
                crate::utility::qstring_to_int(properties.get("size").map_or("", String::as_str))
            },
            file_id: properties
                .get("id")
                .map(|v| v.as_bytes().to_vec())
                .unwrap_or_default(),
            ..Self::default()
        };
        if let Some(p) = properties.get("permissions") {
            item.remote_perm = RemotePermissions::from_server_string_with(p, algorithm, properties);
        }
        if properties.get("share-types").is_some_and(|v| !v.is_empty()) {
            item.remote_perm.set_permission(Permission::IsShared);
        }
        item.is_shared = item.remote_perm.has_permission(Permission::IsShared);
        item.last_share_state_fetched_timestamp = current_msecs_since_epoch();
        item.e2e_encryption_status =
            if properties.get("is-encrypted").map(String::as_str) == Some("1") {
                ItemEncryptionStatus::EncryptedMigratedV2_0
            } else {
                ItemEncryptionStatus::NotEncrypted
            };
        if item.is_encrypted() {
            item.e2e_encryption_server_capability = item.e2e_encryption_status;
        }
        item.locked = if properties.get("lock").map(String::as_str) == Some("1") {
            LockStatus::LockedItem
        } else {
            LockStatus::UnlockedItem
        };
        let get = |k: &str| properties.get(k).cloned().unwrap_or_default();
        item.lock_owner_display_name = get("lock-owner-displayname");
        item.lock_owner_id = get("lock-owner");
        item.lock_editor_app = get("lock-owner-editor");
        item.lock_owner_type = get("lock-owner-type")
            .parse::<u64>()
            .map(|v| LockOwnerType::from_i64(v as i64))
            .unwrap_or(LockOwnerType::UserLock);
        item.lock_time = get("lock-time")
            .parse::<u64>()
            .map(|v| v as i64)
            .unwrap_or(0);
        item.lock_timeout = get("lock-timeout")
            .parse::<u64>()
            .map(|v| v as i64)
            .unwrap_or(0);
        item.lock_token = get("lock-token");
        let date = crate::utility::parse_rfc2822(&get("getlastmodified").replace("GMT", "+0000"));
        if let Some(secs) = date
            && secs > 0
        {
            item.modtime = secs;
        }
        if let Some(e) = properties.get("getetag") {
            item.etag = nc_dav::xml::parse_etag(e.as_bytes());
        }
        if let Some(c) = properties.get("checksums") {
            item.checksum_header = nc_journal::checksums::find_best_checksum(c.as_bytes());
        }
        if let Some(l) = properties.get("metadata-files-live-photo") {
            item.is_live_photo = true;
            item.live_photo_file = l.clone();
        }
        if is_directory
            && properties.contains_key("quota-used-bytes")
            && properties.contains_key("quota-available-bytes")
        {
            item.folder_quota.bytes_used =
                crate::utility::qstring_to_long_long(&get("quota-used-bytes"));
            item.folder_quota.bytes_available =
                crate::utility::qstring_to_long_long(&get("quota-available-bytes"));
        }
        item.direction = Direction::None;
        item.instruction = Instruction::None;
        item
    }

    /// `updateLockStateFromDbRecord(dbRecord)`.
    pub fn update_lock_state_from_db_record(&mut self, rec: &SyncJournalFileRecord) {
        self.locked = if rec.lockstate.locked {
            LockStatus::LockedItem
        } else {
            LockStatus::UnlockedItem
        };
        self.lock_owner_id = rec.lockstate.lock_owner_id.clone();
        self.lock_owner_display_name = rec.lockstate.lock_owner_display_name.clone();
        self.lock_owner_type = LockOwnerType::from_i64(rec.lockstate.lock_owner_type);
        self.lock_editor_app = rec.lockstate.lock_editor_app.clone();
        self.lock_time = rec.lockstate.lock_time;
        self.lock_timeout = rec.lockstate.lock_timeout;
        self.lock_token = rec.lockstate.lock_token.clone();
    }
}

/// `operator<(SyncFileItem, SyncFileItem)`: sorts by destination so that a
/// folder is directly followed by its contents ("foo", "foo/bar",
/// "foo-bar"), comparing UTF-16 code units like `QString`.
pub fn destination_less(d1: &str, d2: &str) -> bool {
    let a: Vec<u16> = d1.encode_utf16().collect();
    let b: Vec<u16> = d2.encode_utf16().collect();
    let min = a.len().min(b.len());
    let mut prefix = 0;
    while prefix < min && a[prefix] == b[prefix] {
        prefix += 1;
    }
    if prefix == b.len() {
        return false;
    }
    if prefix == a.len() {
        return true;
    }
    let slash = u16::from(b'/');
    if a[prefix] == slash {
        return true;
    }
    if b[prefix] == slash {
        return false;
    }
    a[prefix] < b[prefix]
}

/// Total order of items by [`destination_less`].
pub fn item_ordering(a: &SyncFileItem, b: &SyncFileItem) -> Ordering {
    let (da, db) = (a.destination(), b.destination());
    if destination_less(da, db) {
        Ordering::Less
    } else if destination_less(db, da) {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_destination_ordering_puts_children_after_folder() {
        let mut v = vec!["foo-bar", "foo/bar", "foo", "A/b", "A", "a"];
        v.sort_by(|a, b| {
            if destination_less(a, b) {
                Ordering::Less
            } else if destination_less(b, a) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        assert_eq!(v, ["A", "A/b", "a", "foo", "foo/bar", "foo-bar"]);
    }
}
