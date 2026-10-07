// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud GmbH
// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/discovery.{h,cpp}` (ProcessDirectoryJob) and
// `src/libsync/discoveryphase.{h,cpp}` (DiscoveryPhase,
// DiscoverySingleDirectoryJob, DiscoverySingleLocalDirectoryJob) of
// nextcloud/desktop v34.0.5.

//! The discovery phase: lists the server (PROPFIND) and the local folder,
//! merges both with the journal and decides, for every entry, what the
//! propagator has to do (reconcile).
//!
//! # Execution model
//!
//! Upstream runs on the Qt event loop: network replies and the local
//! directory listings (thread pool) arrive as queued signals, a few things
//! are posted with `QTimer::singleShot(0)`, everything else is a direct
//! call. Here the [`DiscoveryPhase`] owns an arena of
//! [`ProcessDirectoryJob`]s (`JobId` indices replace the `QObject` tree),
//! the in-flight network requests (futures that resolve to a
//! [`Completion`]) and a queue of posted events. [`DiscoveryPhase::run`]
//! drains the posted events, then waits for the next completion, exactly
//! one at a time, so every handler runs to completion before the next one
//! starts, like a Qt slot. Local listings are done synchronously when the
//! job starts and delivered through the posted queue (a queued signal).
//!
//! Virtual files and end-to-end encryption are out of scope: the VFS is
//! always "off" (every VFS-only branch upstream is dead code here) and
//! encrypted folders are handled like the official client does when E2EE is
//! not set up (they are added to the selective sync blacklist).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use futures_util::future::LocalBoxFuture;
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use nc_dav::xml::PropertyMap;
use nc_dav::{Account, HttpError, JobOptions, Target};
use nc_journal::csync::{ItemEncryptionStatus, ItemType};
use nc_journal::exclude::{ExcludeType, ExcludedFiles};
use nc_journal::journal::{ConflictRecord, FolderQuota, SyncJournalFileRecord};
use nc_journal::journal::{SelectiveSyncListType, SyncJournalDb, find_path_in_selective_sync_list};
use nc_journal::remote_permissions::{MountedPermissionAlgorithm, Permission, RemotePermissions};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::filesystem;
use crate::item::{
    Direction, ErrorCategory, Instruction, LockOwnerType, LockStatus, Status, SyncFileItem,
    SyncFileItemPtr, SynchronizationOptions, current_msecs_since_epoch, from_db_encryption_status,
    from_end_to_end_encryption_api_version, new_item,
};
use crate::options::SyncOptions;
use crate::utility::{normalize_etag, qstring_cmp, qstring_to_double_i64, qstring_to_int};

const LOG: &str = "nextcloud.sync.discovery";

/// `delayIntervalForSyncRetryForFilesExceedQuotaSeconds`.
const DELAY_INTERVAL_FOR_SYNC_RETRY_FOR_FILES_EXCEED_QUOTA_SECONDS: i64 = 60;

/// `LocalDiscoveryStyle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalDiscoveryStyle {
    /// read all local data from the filesystem
    #[default]
    FilesystemOnly,
    /// read from the db, except for listed paths
    DatabaseAndFilesystem,
}

/// `LocalInfo`: one entry of a local directory listing.
#[derive(Clone, Debug, Default)]
pub struct LocalInfo {
    /// Whether the entry exists (upstream: `name` not null).
    pub valid: bool,
    /// File name of the entry (no directory part).
    pub name: String,
    pub case_clash_conflicting_name: String,
    pub modtime: i64,
    pub size: i64,
    pub inode: u64,
    pub item_type: ItemType,
    pub is_directory: bool,
    pub is_hidden: bool,
    pub is_virtual_file: bool,
    pub is_sym_link: bool,
    pub is_metadata_missing: bool,
    pub is_permissions_invalid: bool,
    pub is_locked: bool,
}

impl LocalInfo {
    pub fn is_valid(&self) -> bool {
        self.valid
    }
}

/// `RemoteInfo`: one entry of a server listing.
#[derive(Clone, Debug, Default)]
pub struct RemoteInfo {
    /// Whether the entry exists (upstream: `name` not null).
    pub valid: bool,
    pub name: String,
    pub etag: Vec<u8>,
    pub file_id: Vec<u8>,
    pub checksum_header: Vec<u8>,
    pub remote_perm: RemotePermissions,
    pub modtime: i64,
    pub size: i64,
    pub size_of_folder: i64,
    pub is_directory: bool,
    pub is_e2e_encrypted: bool,
    pub is_file_drop_detected: bool,
    pub e2e_mangled_name: String,
    pub shared_by_me: bool,
    pub direct_download_url: String,
    pub direct_download_cookies: String,
    pub locked: LockStatus,
    pub lock_owner_display_name: String,
    pub lock_owner_id: String,
    pub lock_owner_type: LockOwnerType,
    pub lock_editor_app: String,
    pub lock_time: i64,
    pub lock_timeout: i64,
    pub lock_token: String,
    pub is_live_photo: bool,
    pub live_photo_file: String,
    pub folder_quota: FolderQuota,
}

impl RemoteInfo {
    pub fn is_valid(&self) -> bool {
        self.valid
    }
    pub fn is_e2e_encrypted(&self) -> bool {
        self.is_e2e_encrypted
    }
}

/// `LsColJob::propertyMapToRemoteInfo`.
pub fn property_map_to_remote_info(
    map: &PropertyMap,
    algorithm: MountedPermissionAlgorithm,
    result: &mut RemoteInfo,
) {
    // QMap iteration: sorted by key ("share-types" comes after "permissions").
    for (property, value) in map {
        match property.as_str() {
            "resourcetype" => result.is_directory = value.contains("collection"),
            "getlastmodified" => {
                result.modtime = 0;
                if let Some(secs) = crate::utility::parse_rfc2822(&value.replace("GMT", "+0000"))
                    && secs > 0
                {
                    result.modtime = secs;
                }
            }
            "getcontentlength" => {
                // See #4573, sometimes negative size values are returned
                result.size = match value.trim().parse::<i64>() {
                    Ok(ll) if ll >= 0 => ll,
                    _ => 0,
                };
            }
            "getetag" => result.etag = normalize_etag(value.as_bytes()),
            "id" => result.file_id = value.as_bytes().to_vec(),
            "downloadURL" => result.direct_download_url = value.clone(),
            "dDC" => result.direct_download_cookies = value.clone(),
            "permissions" => {
                result.remote_perm =
                    RemotePermissions::from_server_string_with(value, algorithm, map)
            }
            "checksums" => {
                result.checksum_header = nc_journal::checksums::find_best_checksum(value.as_bytes())
            }
            "share-types" if !value.is_empty() => {
                if result.remote_perm.is_null() {
                    log::warn!("Server returned a share type, but no permissions?");
                } else {
                    // S means shared with me. But for our purpose, we want to
                    // know if the file is shared. It does not matter if we
                    // are the owner or not. Piggy back on the permission field
                    result.remote_perm.set_permission(Permission::IsShared);
                    result.shared_by_me = true;
                }
            }
            "is-encrypted" if value == "1" => result.is_e2e_encrypted = true,
            "lock" => {
                result.locked = if value == "1" {
                    LockStatus::LockedItem
                } else {
                    LockStatus::UnlockedItem
                }
            }
            _ => {}
        }
        match property.as_str() {
            "lock-owner-displayname" => result.lock_owner_display_name = value.clone(),
            "lock-owner" => result.lock_owner_id = value.clone(),
            "lock-owner-type" => {
                result.lock_owner_type = value
                    .trim()
                    .parse::<u64>()
                    .map(|v| LockOwnerType::from_i64(v as i64))
                    .unwrap_or(LockOwnerType::UserLock)
            }
            "lock-owner-editor" => result.lock_editor_app = value.clone(),
            "lock-time" => result.lock_time = value.trim().parse::<u64>().map_or(0, |v| v as i64),
            "lock-timeout" => {
                result.lock_timeout = value.trim().parse::<u64>().map_or(0, |v| v as i64)
            }
            "lock-token" => result.lock_token = value.clone(),
            "metadata-files-live-photo" => {
                result.live_photo_file = value.clone();
                result.is_live_photo = true;
            }
            _ => {}
        }
    }
    if result.is_directory && map.contains_key("size") {
        result.size_of_folder = qstring_to_int(&map["size"]);
    }
    if result.is_directory
        && map.contains_key("quota-used-bytes")
        && map.contains_key("quota-available-bytes")
    {
        // The server can respond with e.g. "2.58440798353E+12" for the quota
        result.folder_quota.bytes_used =
            qstring_to_double_i64(&map["quota-used-bytes"]).unwrap_or(-1);
        result.folder_quota.bytes_available =
            qstring_to_double_i64(&map["quota-available-bytes"]).unwrap_or(-1);
    }
}

/// `adjustRenamedPath(renamedItems, original)`: maps a path through the
/// renames of its parent directories.
pub fn adjust_renamed_path(renamed_items: &BTreeMap<String, String>, original: &str) -> String {
    let mut slash_pos = original.len();
    while let Some(pos) = original[..slash_pos].rfind('/') {
        if pos == 0 {
            break;
        }
        if let Some(target) = renamed_items.get(&original[..pos]) {
            return format!("{}{}", target, &original[pos..]);
        }
        slash_pos = pos;
    }
    original.to_owned()
}

/// `ProcessDirectoryJob::PathTuple`: a path during discovery, which may
/// differ locally and on the server in case of renames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathTuple {
    /// Path as in the DB (before the sync)
    pub original: String,
    /// Path that will be the result after the sync (and will be in the DB)
    pub target: String,
    /// Path on the server (before the sync)
    pub server: String,
    /// Path locally (before the sync)
    pub local: String,
}

impl PathTuple {
    pub fn path_append(base: &str, name: &str) -> String {
        if base.is_empty() {
            name.to_owned()
        } else {
            format!("{base}/{name}")
        }
    }

    pub fn add_name(&self, name: &str) -> PathTuple {
        PathTuple {
            original: Self::path_append(&self.original, name),
            target: Self::path_append(&self.target, name),
            server: Self::path_append(&self.server, name),
            local: Self::path_append(&self.local, name),
        }
    }
}

/// `ProcessDirectoryJob::QueryMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueryMode {
    #[default]
    NormalQuery,
    /// Do not query this folder because it does not exist
    ParentDontExist,
    /// No need to query this folder because it has not changed from what is in the DB
    ParentNotChanged,
    /// Do not query this folder because it is in the blacklist (remote entries only)
    InBlackList,
}

/// Index of a [`ProcessDirectoryJob`] in the arena.
pub type JobId = usize;

/// `ProcessDirectoryJob::Entries`.
#[derive(Clone, Debug, Default)]
struct Entries {
    name_override: String,
    db_entry: SyncJournalFileRecord,
    server_entry: RemoteInfo,
    local_entry: LocalInfo,
}

/// A `QString` key ordered like `QString::operator<` (UTF-16 code units).
#[derive(Clone, Debug, PartialEq, Eq)]
struct QKey(String);

impl Ord for QKey {
    fn cmp(&self, other: &Self) -> Ordering {
        qstring_cmp(&self.0, &other.0)
    }
}

impl PartialOrd for QKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Result of the server query of one directory (`DiscoverySingleDirectoryJob`).
#[derive(Debug)]
struct ServerListing {
    result: Result<Vec<RemoteInfo>, HttpError>,
    data_fingerprint: Vec<u8>,
    first_permissions: Option<RemotePermissions>,
    first_file_id: Option<i64>,
    folder_quota: Option<FolderQuota>,
    first_etag: Vec<u8>,
}

/// One directory being discovered (`ProcessDirectoryJob`).
#[derive(Debug)]
pub struct ProcessDirectoryJob {
    pub dir_item: Option<SyncFileItemPtr>,
    pub dir_parent_item: Option<SyncFileItemPtr>,
    parent: Option<JobId>,
    last_sync_timestamp: i64,
    query_server: QueryMode,
    query_local: QueryMode,
    server_normal_query_entries: Vec<RemoteInfo>,
    local_normal_query_entries: Vec<LocalInfo>,
    server_query_done: bool,
    local_query_done: bool,
    root_permissions: RemotePermissions,
    /// Number of currently running async jobs (not the sub directories).
    pending_async_jobs: i32,
    queued_jobs: VecDeque<JobId>,
    running_jobs: Vec<JobId>,
    current_folder: PathTuple,
    /// the directory contains modified item what would prevent deletion
    child_modified: bool,
    /// The directory contains ignored item that would prevent deletion
    child_ignored: bool,
    is_inside_encrypted_tree: bool,
    folder_quota: FolderQuota,
    is_root: bool,
    /// The server query is in flight (upstream `_serverJob`).
    server_job_running: bool,
}

/// Something the discovery waits for.
enum Completion {
    ServerListing {
        job: JobId,
        listing: Box<ServerListing>,
    },
    /// The `RequestEtagJob` of a remote rename candidate (down).
    RenameEtagDown {
        job: JobId,
        ctx: Box<RenameDownCtx>,
        result: Result<Vec<u8>, HttpError>,
    },
    /// The `RequestEtagJob` of a local rename candidate (up).
    RenameEtagUp {
        job: JobId,
        ctx: Box<RenameUpCtx>,
        result: Result<Vec<u8>, HttpError>,
    },
    /// The PROPFIND of `checkFolderSizeLimit` for a new folder.
    NewFolderSize {
        job: JobId,
        ctx: Box<NewFolderCtx>,
        size: Option<i64>,
    },
    /// The PROPFIND of `checkSelectiveSyncExistingFolder`.
    ExistingFolderSize { path: String, size: Option<i64> },
}

/// Posted (queued) events.
enum Posted {
    ScheduleMoreJobs,
    LocalListing {
        job: JobId,
        result: LocalListingResult,
    },
}

enum LocalListingResult {
    Finished(Vec<LocalInfo>),
    FatalError(String),
    NonFatalError(String),
}

struct RenameDownCtx {
    item: SyncFileItemPtr,
    path: PathTuple,
    local_entry: LocalInfo,
    server_entry: RemoteInfo,
    db_entry: SyncJournalFileRecord,
    base: SyncJournalFileRecord,
    original_path: String,
}

struct RenameUpCtx {
    item: SyncFileItemPtr,
    path: PathTuple,
    server_entry: RemoteInfo,
    db_entry: SyncJournalFileRecord,
    base: SyncJournalFileRecord,
    original_path: String,
    recurse_query_server: QueryMode,
}

struct NewFolderCtx {
    item: SyncFileItemPtr,
    path: PathTuple,
    local_entry: LocalInfo,
    server_entry: RemoteInfo,
    db_entry: SyncJournalFileRecord,
    folder_path: String,
}

/// The signals the discovery emits (connected to the sync engine).
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub struct DiscoveryCallbacks {
    pub item_discovered: Option<Box<dyn FnMut(&SyncFileItemPtr)>>,
    /// A new folder was discovered and was not synced because of the
    /// confirmation feature (path, is external storage).
    pub new_big_folder: Option<Box<dyn FnMut(&str, bool)>>,
    pub existing_folder_now_big: Option<Box<dyn FnMut(&str)>>,
    /// For excluded items that don't show up in `item_discovered`.
    pub silently_excluded: Option<Box<dyn FnMut(&str)>>,
    pub remnant_read_only_folder_discovered: Option<Box<dyn FnMut(&SyncFileItemPtr)>>,
    pub seen_locked_file: Option<Box<dyn FnMut(&str)>>,
    /// The root etag (and the server `Date`).
    pub root_etag: Option<Box<dyn FnMut(&[u8], &[u8])>>,
    pub root_file_id_received: Option<Box<dyn FnMut(i64)>>,
    pub updated_root_folder_quota: Option<Box<dyn FnMut(i64, i64)>>,
}

/// The discovery phase (`DiscoveryPhase`).
pub struct DiscoveryPhase {
    // ---- input
    /// absolute path to the local directory. ends with '/'
    pub local_dir: String,
    /// remote folder, ends with '/'
    pub remote_folder: String,
    pub statedb: Arc<SyncJournalDb>,
    pub account: Arc<Account>,
    pub sync_options: SyncOptions,
    pub excludes: Rc<RefCell<ExcludedFiles>>,
    pub invalid_filename_rx: Option<Regex>,
    /// The blacklist from the capabilities
    pub server_blacklisted_files: Vec<String>,
    pub leading_and_trailing_spaces_files_allowed: Vec<String>,
    pub should_enforce_windows_file_name_compatibility: bool,
    pub ignore_hidden_files: bool,
    pub should_discover_locally: Box<dyn Fn(&str) -> bool>,
    // ---- output
    pub data_fingerprint: Vec<u8>,
    pub another_sync_needed: bool,
    pub files_needing_scheduled_sync: HashMap<String, i64>,
    pub files_unschedule_sync: Vec<String>,
    pub list_exclusive_files: Vec<String>,
    pub forbidden_filenames: Vec<String>,
    pub forbidden_basenames: Vec<String>,
    pub forbidden_extensions: Vec<String>,
    pub forbidden_chars: Vec<String>,
    pub has_upload_error_items: bool,
    pub has_download_removed_items: bool,
    pub no_case_conflict_records_in_db: bool,
    pub file_system_reliable_permissions: bool,
    pub top_level_e2ee_folder_paths: HashSet<String>,
    pub callbacks: DiscoveryCallbacks,

    // ---- private state
    current_root_job: Option<JobId>,
    /// Maps the db-path of a deleted item to its SyncFileItem.
    deleted_item: BTreeMap<String, SyncFileItemPtr>,
    directory_names_to_restore_on_propagation: Vec<String>,
    /// Maps the db-path of a deleted folder to its queued job.
    queued_deleted_directories: BTreeMap<String, JobId>,
    /// map source (original path) -> destinations (current server or local path)
    renamed_items_remote: BTreeMap<String, String>,
    renamed_items_local: BTreeMap<String, String>,
    /// set of paths that should not be removed even though they are removed
    /// locally (keys end with '/').
    forbidden_deletes: BTreeMap<String, bool>,
    currently_active_jobs: i32,
    selective_sync_black_list: Vec<String>,
    selective_sync_white_list: Vec<String>,
    permanent_deletion_requests: HashSet<String>,

    jobs: Vec<Option<ProcessDirectoryJob>>,
    pending: FuturesUnordered<LocalBoxFuture<'static, (u64, Completion)>>,
    posted: VecDeque<(u64, Posted)>,
    /// Completions already available, keyed by the sequence number of their
    /// request (see [`DiscoveryPhase::run`]).
    ready: BTreeMap<u64, Completion>,
    /// Sequence number of the next request or posted event.
    next_seq: u64,
    fatal: Option<(String, ErrorCategory)>,
    finished: bool,
}

/// What [`DiscoveryPhase::run`] returns on failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    /// `fatalError(errorString, category)`.
    Fatal(String, ErrorCategory),
    /// The sync was aborted.
    Aborted,
}

/// How the root job is set up.
pub struct RootJobSpec {
    pub path: PathTuple,
    pub dir_item: Option<SyncFileItemPtr>,
    pub query_local: QueryMode,
    pub last_sync_timestamp: i64,
}

impl DiscoveryPhase {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local_dir: String,
        remote_folder: String,
        statedb: Arc<SyncJournalDb>,
        account: Arc<Account>,
        sync_options: SyncOptions,
        excludes: Rc<RefCell<ExcludedFiles>>,
    ) -> Self {
        Self {
            local_dir,
            remote_folder,
            statedb,
            account,
            sync_options,
            excludes,
            invalid_filename_rx: None,
            server_blacklisted_files: Vec::new(),
            leading_and_trailing_spaces_files_allowed: Vec::new(),
            should_enforce_windows_file_name_compatibility: false,
            ignore_hidden_files: false,
            should_discover_locally: Box::new(|_| true),
            data_fingerprint: Vec::new(),
            another_sync_needed: false,
            files_needing_scheduled_sync: HashMap::new(),
            files_unschedule_sync: Vec::new(),
            list_exclusive_files: Vec::new(),
            forbidden_filenames: Vec::new(),
            forbidden_basenames: Vec::new(),
            forbidden_extensions: Vec::new(),
            forbidden_chars: Vec::new(),
            has_upload_error_items: false,
            has_download_removed_items: false,
            no_case_conflict_records_in_db: false,
            file_system_reliable_permissions: false,
            top_level_e2ee_folder_paths: HashSet::new(),
            callbacks: DiscoveryCallbacks::default(),
            current_root_job: None,
            deleted_item: BTreeMap::new(),
            directory_names_to_restore_on_propagation: Vec::new(),
            queued_deleted_directories: BTreeMap::new(),
            renamed_items_remote: BTreeMap::new(),
            renamed_items_local: BTreeMap::new(),
            forbidden_deletes: BTreeMap::new(),
            currently_active_jobs: 0,
            selective_sync_black_list: Vec::new(),
            selective_sync_white_list: Vec::new(),
            permanent_deletion_requests: HashSet::new(),
            jobs: Vec::new(),
            pending: FuturesUnordered::new(),
            posted: VecDeque::new(),
            ready: BTreeMap::new(),
            next_seq: 0,
            fatal: None,
            finished: false,
        }
    }

    // ------------------------------------------------------------------
    // DiscoveryPhase helpers

    /// `setSelectiveSyncBlackList`.
    pub fn set_selective_sync_black_list(&mut self, list: Vec<String>) {
        let mut list = list;
        list.sort_by(|a, b| qstring_cmp(a, b));
        self.selective_sync_black_list = list;
    }

    /// `setSelectiveSyncWhiteList`.
    pub fn set_selective_sync_white_list(&mut self, list: Vec<String>) {
        let mut list = list;
        list.sort_by(|a, b| qstring_cmp(a, b));
        self.selective_sync_white_list = list;
    }

    /// `isInSelectiveSyncBlackList`.
    fn is_in_selective_sync_black_list(&self, path: &str) -> bool {
        if self.selective_sync_black_list.is_empty() {
            // If there is no black list, everything is allowed
            return false;
        }
        find_path_in_selective_sync_list(&self.selective_sync_black_list, path)
    }

    /// `activeFolderSizeLimit` (the VFS is always off).
    fn active_folder_size_limit(&self) -> bool {
        self.sync_options.new_big_folder_size_limit > 0
    }

    /// `notifyExistingFolderOverLimit`: also needs the
    /// `notifyExistingFoldersOverLimit` setting of the GUI configuration
    /// (`SyncOptions::notify_existing_folders_over_limit` in the port).
    fn notify_existing_folder_over_limit(&self) -> bool {
        self.active_folder_size_limit() && self.sync_options.notify_existing_folders_over_limit
    }

    /// `isRenamed`.
    fn is_renamed(&self, p: &str) -> bool {
        self.renamed_items_local.contains_key(p) || self.renamed_items_remote.contains_key(p)
    }

    /// `adjustRenamedPath(original, direction)`.
    fn adjust_renamed_path(&self, original: &str, d: Direction) -> String {
        if d == Direction::Down {
            adjust_renamed_path(&self.renamed_items_remote, original)
        } else {
            adjust_renamed_path(&self.renamed_items_local, original)
        }
    }

    fn emit_item_discovered(&mut self, item: &SyncFileItemPtr) {
        // DiscoveryPhase::slotItemDiscovered
        {
            let i = item.borrow();
            if i.instruction == Instruction::Error && i.direction == Direction::Up {
                self.has_upload_error_items = true;
            }
            if i.instruction == Instruction::Remove && i.direction == Direction::Down {
                self.has_download_removed_items = true;
            }
        }
        if let Some(cb) = self.callbacks.item_discovered.as_mut() {
            cb(item);
        }
    }

    fn emit_silently_excluded(&mut self, path: &str) {
        if let Some(cb) = self.callbacks.silently_excluded.as_mut() {
            cb(path);
        }
    }

    fn emit_fatal_error(&mut self, msg: String, category: ErrorCategory) {
        log::warn!(target: LOG, "fatal error: {msg}");
        if self.fatal.is_none() {
            self.fatal = Some((msg, category));
        }
    }

    /// `findAndCancelDeletedJob`: if the db-path is scheduled for deletion,
    /// cancel it and return the old etag.
    fn find_and_cancel_deleted_job(&mut self, original_path: &str) -> (bool, Vec<u8>) {
        let mut result = false;
        let mut old_etag = Vec::new();
        if let Some(it) = self.deleted_item.remove(original_path) {
            let mut item = it.borrow_mut();
            let instruction = item.instruction;
            if instruction == Instruction::Ignore && item.item_type == ItemType::VirtualFile {
                // re-creation of virtual files count as a delete
                result = true;
                old_etag = item.etag.clone();
            } else {
                if !(instruction == Instruction::Remove
                    || instruction == Instruction::Ignore
                    || (item.item_type == ItemType::VirtualFile && instruction == Instruction::New)
                    || (item.is_restoration && instruction == Instruction::New))
                {
                    log::warn!(target: LOG, "ENFORCE(FAILING) {original_path} instruction {instruction:?}");
                    drop(item);
                    self.emit_fatal_error(
                        format!("Error while canceling deletion of {original_path}"),
                        ErrorCategory::GenericError,
                    );
                    item = it.borrow_mut();
                }
                item.instruction = Instruction::None;
                result = true;
                old_etag = item.etag.clone();
            }
        }
        if let Some(other_job) = self.queued_deleted_directories.remove(original_path) {
            if let Some(job) = self.jobs[other_job].take()
                && let Some(d) = &job.dir_item
            {
                old_etag = d.borrow().etag.clone();
            }
            result = true;
        }
        (result, old_etag)
    }

    /// `enqueueDirectoryToDelete`.
    fn enqueue_directory_to_delete(&mut self, path: &str, job_id: JobId) {
        self.queued_deleted_directories
            .insert(path.to_owned(), job_id);
        if let Some(job) = self.jobs[job_id].as_ref()
            && let Some(d) = &job.dir_item
        {
            let d = d.borrow();
            if d.is_restoration
                && d.direction == Direction::Down
                && d.instruction == Instruction::New
            {
                self.directory_names_to_restore_on_propagation
                    .push(path.to_owned());
            }
        }
    }

    /// `recursiveCheckForDeletedParents`.
    fn recursive_check_for_deleted_parents(&self, item_path: &str) -> bool {
        let mut current_parent_folder = String::new();
        for component in item_path.split('/') {
            if !current_parent_folder.is_empty() {
                current_parent_folder.push('/');
            }
            current_parent_folder.push_str(component);
            if self.deleted_item.contains_key(&current_parent_folder) {
                return true;
            }
        }
        false
    }

    /// `markPermanentDeletionRequests`.
    fn mark_permanent_deletion_requests(&mut self) {
        let requests: Vec<String> = self.permanent_deletion_requests.iter().cloned().collect();
        for original_path in requests {
            let Some(item) = self.deleted_item.get(&original_path) else {
                log::warn!(target: LOG, "didn't find an item for {original_path} (yet)");
                continue;
            };
            let mut item = item.borrow_mut();
            if !(item.instruction == Instruction::Remove || item.direction == Direction::Up) {
                continue;
            }
            item.wants_specific_actions = SynchronizationOptions::WantsPermanentDeletion;
        }
    }

    // ------------------------------------------------------------------
    // Running

    /// Creates the root job (`ProcessDirectoryJob(data, basePinState, ...)`).
    pub fn create_root_job(&mut self, spec: RootJobSpec) -> JobId {
        let job = ProcessDirectoryJob {
            dir_item: spec.dir_item,
            dir_parent_item: None,
            parent: None,
            last_sync_timestamp: spec.last_sync_timestamp,
            query_server: QueryMode::NormalQuery,
            query_local: spec.query_local,
            server_normal_query_entries: Vec::new(),
            local_normal_query_entries: Vec::new(),
            server_query_done: false,
            local_query_done: false,
            root_permissions: RemotePermissions::null(),
            pending_async_jobs: 0,
            queued_jobs: VecDeque::new(),
            running_jobs: Vec::new(),
            current_folder: spec.path,
            child_modified: false,
            child_ignored: false,
            is_inside_encrypted_tree: false,
            folder_quota: FolderQuota::default(),
            is_root: true,
            server_job_running: false,
        };
        self.jobs.push(Some(job));
        self.jobs.len() - 1
    }

    /// Runs the discovery to completion (`startJob(root)` until `finished`).
    pub async fn run(
        &mut self,
        root: JobId,
        abort: &CancellationToken,
    ) -> Result<(), DiscoveryError> {
        self.start_job(root);
        loop {
            if let Some((msg, cat)) = self.fatal.take() {
                return Err(DiscoveryError::Fatal(msg, cat));
            }
            if self.finished {
                return Ok(());
            }
            if abort.is_cancelled() {
                return Err(DiscoveryError::Aborted);
            }
            // Upstream's event queue is FIFO and FakeQNAM answers with a
            // queued event posted when the request is sent, so a reply that
            // is already available is delivered before the events posted
            // after its request (e.g. the `RequestEtagJob` of a move is
            // answered before `scheduleMoreJobs` starts the next directory).
            while let Some(Some((seq, c))) = self.pending.next().now_or_never() {
                self.ready.insert(seq, c);
            }
            let next_posted = self.posted.front().map(|(seq, _)| *seq);
            let next_ready = self.ready.keys().next().copied();
            match (next_posted, next_ready) {
                (Some(p), Some(r)) if r < p => {
                    let c = self.ready.remove(&r).expect("ready completion");
                    self.handle_completion(c);
                    continue;
                }
                (Some(_), _) => {
                    let (_, ev) = self.posted.pop_front().expect("posted event");
                    self.handle_posted(ev);
                    continue;
                }
                (None, Some(r)) => {
                    let c = self.ready.remove(&r).expect("ready completion");
                    self.handle_completion(c);
                    continue;
                }
                (None, None) => {}
            }
            let next = tokio::select! {
                biased;
                _ = abort.cancelled() => return Err(DiscoveryError::Aborted),
                c = self.pending.next() => c,
            };
            match next {
                Some((_, c)) => self.handle_completion(c),
                None => {
                    return Err(DiscoveryError::Fatal(
                        "Discovery stalled: nothing left to wait for".to_owned(),
                        ErrorCategory::GenericError,
                    ));
                }
            }
        }
    }

    /// `startJob(job)`.
    fn start_job(&mut self, job: JobId) {
        debug_assert!(self.current_root_job.is_none());
        self.current_root_job = Some(job);
        self.job_start(job);
    }

    /// The root job's `finished` handler installed by `startJob`.
    fn root_job_finished(&mut self, job: JobId) {
        self.current_root_job = None;
        let dir_item = self.jobs[job].as_ref().and_then(|j| j.dir_item.clone());
        if let Some(d) = dir_item {
            self.emit_item_discovered(&d);
        }
        self.jobs[job] = None;
        // Once the main job has finished recurse here to execute the remaining
        // jobs for queued deleted directories.
        if let Some((key, next)) = self.queued_deleted_directories.pop_first() {
            let _ = key;
            self.start_job(next);
        } else {
            self.mark_permanent_deletion_requests();
            self.finished = true;
        }
    }

    /// `scheduleMoreJobs`.
    fn schedule_more_jobs(&mut self) {
        let limit = self.sync_options.parallel_network_jobs.max(1);
        if let Some(root) = self.current_root_job
            && self.currently_active_jobs < limit
        {
            let n = limit - self.currently_active_jobs;
            self.process_sub_jobs(root, n);
        }
    }

    /// Queues an in-flight request; its completion keeps the request's place
    /// in the event order.
    fn push_pending(&mut self, fut: LocalBoxFuture<'static, Completion>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push(Box::pin(async move { (seq, fut.await) }));
    }

    /// `QTimer::singleShot(0)` / a queued signal.
    fn push_posted(&mut self, ev: Posted) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.posted.push_back((seq, ev));
    }

    fn post_schedule_more_jobs(&mut self) {
        self.push_posted(Posted::ScheduleMoreJobs);
    }

    fn handle_posted(&mut self, ev: Posted) {
        match ev {
            Posted::ScheduleMoreJobs => self.schedule_more_jobs(),
            Posted::LocalListing { job, result } => self.local_listing_finished(job, result),
        }
    }

    fn handle_completion(&mut self, c: Completion) {
        match c {
            Completion::ServerListing { job, listing } => {
                self.server_listing_finished(job, *listing)
            }
            Completion::RenameEtagDown { job, ctx, result } => {
                if self.jobs.get(job).is_some_and(Option::is_some) {
                    self.rename_etag_down_finished(job, *ctx, result);
                }
            }
            Completion::RenameEtagUp { job, ctx, result } => {
                if self.jobs.get(job).is_some_and(Option::is_some) {
                    self.rename_etag_up_finished(job, *ctx, result);
                }
            }
            Completion::NewFolderSize { job, ctx, size } => {
                if self.jobs.get(job).is_some_and(Option::is_some) {
                    let big =
                        size.is_some_and(|s| s >= self.sync_options.new_big_folder_size_limit);
                    let blocked = self.new_folder_size_checked(&ctx.folder_path, big);
                    self.new_folder_check_done(job, *ctx, blocked);
                }
            }
            Completion::ExistingFolderSize { path, size } => {
                let big = size.is_some_and(|s| s >= self.sync_options.new_big_folder_size_limit);
                if big && let Some(cb) = self.callbacks.existing_folder_now_big.as_mut() {
                    cb(&path);
                }
            }
        }
    }

    fn job(&self, id: JobId) -> &ProcessDirectoryJob {
        self.jobs[id].as_ref().expect("live job")
    }

    fn job_mut(&mut self, id: JobId) -> &mut ProcessDirectoryJob {
        self.jobs[id].as_mut().expect("live job")
    }

    fn alive(&self, id: JobId) -> bool {
        self.jobs.get(id).is_some_and(Option::is_some)
    }

    /// Emits `finished()` of a job to whoever is connected: the parent's
    /// `subJobFinished`, or `startJob`'s handler for a root job.
    fn job_emit_finished(&mut self, id: JobId) {
        let parent = self.job(id).parent;
        match parent {
            Some(p) if self.alive(p) => self.sub_job_finished(p, id),
            Some(_) => {}
            None => {
                if self.current_root_job == Some(id) {
                    self.root_job_finished(id);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // ProcessDirectoryJob

    /// `ProcessDirectoryJob::start`.
    fn job_start(&mut self, id: JobId) {
        log::info!(target: LOG, "STARTING {:?} {:?} {:?} {:?}", self.job(id).current_folder.server, self.job(id).query_server, self.job(id).current_folder.local, self.job(id).query_local);
        self.no_case_conflict_records_in_db =
            self.statedb.case_clash_conflict_record_paths().is_empty();
        if self.job(id).query_server == QueryMode::NormalQuery {
            self.start_async_server_query(id);
        } else {
            self.job_mut(id).server_query_done = true;
        }
        // Check whether a normal local query is even necessary
        if self.job(id).query_local == QueryMode::NormalQuery {
            let cf = self.job(id).current_folder.clone();
            if !(self.should_discover_locally)(&cf.local)
                && (cf.local == cf.original || !(self.should_discover_locally)(&cf.original))
                && !self.is_in_selective_sync_black_list(&cf.original)
            {
                self.job_mut(id).query_local = QueryMode::ParentNotChanged;
            }
        }
        if self.job(id).query_local == QueryMode::NormalQuery {
            self.start_async_local_query(id);
        } else {
            self.job_mut(id).local_query_done = true;
        }
        let j = self.job(id);
        if j.local_query_done && j.server_query_done {
            self.process(id);
        }
    }

    /// `startAsyncServerQuery`: the PROPFIND (`DiscoverySingleDirectoryJob`).
    fn start_async_server_query(&mut self, id: JobId) {
        if let Some(d) = self.job(id).dir_item.clone() {
            let d = d.borrow();
            if d.is_encrypted() && d.encrypted_file_name.is_empty() {
                let p = format!("{}{}", self.remote_folder, d.file);
                self.top_level_e2ee_folder_paths.insert(p);
            }
        }
        let (sub_path, is_root) = {
            let j = self.job(id);
            (
                format!("{}{}", self.remote_folder, j.current_folder.server),
                j.dir_item.is_none(),
            )
        };
        self.currently_active_jobs += 1;
        {
            let j = self.job_mut(id);
            j.pending_async_jobs += 1;
            j.server_job_running = true;
        }
        let account = self.account.clone();
        let fut = async move {
            let listing = discovery_single_directory(&account, &sub_path, is_root).await;
            Completion::ServerListing {
                job: id,
                listing: Box::new(listing),
            }
        };
        self.push_pending(Box::pin(fut));
    }

    /// The `finished` handler of the server query in `startAsyncServerQuery`.
    fn server_listing_finished(&mut self, id: JobId, listing: ServerListing) {
        if !self.alive(id) {
            // The job was deleted meanwhile (its QObject connections are gone).
            self.currently_active_jobs -= 1;
            return;
        }
        // Signals emitted while parsing, before `finished`.
        if let Some(perm) = listing.first_permissions {
            self.job_mut(id).root_permissions = perm;
        }
        if let Some(q) = listing.folder_quota {
            // ProcessDirectoryJob::setFolderQuota
            self.job_mut(id).folder_quota = q;
            if self.job(id).current_folder.original.is_empty()
                && let Some(cb) = self.callbacks.updated_root_folder_quota.as_mut()
            {
                cb(q.bytes_used, q.bytes_available);
            }
        }
        if listing.result.is_ok() {
            if self.job(id).is_root
                && let Some(cb) = self.callbacks.root_etag.as_mut()
            {
                cb(&listing.first_etag, b"");
            }
            if let Some(file_id) = listing.first_file_id
                && self.job(id).is_root
                && let Some(cb) = self.callbacks.root_file_id_received.as_mut()
            {
                cb(file_id);
            }
        }
        self.currently_active_jobs -= 1;
        {
            let j = self.job_mut(id);
            j.pending_async_jobs -= 1;
            j.server_job_running = false;
        }
        match listing.result {
            Ok(results) => {
                {
                    let j = self.job_mut(id);
                    j.server_normal_query_entries = results;
                    j.server_query_done = true;
                }
                if !listing.data_fingerprint.is_empty() && self.data_fingerprint.is_empty() {
                    self.data_fingerprint = listing.data_fingerprint;
                }
                if self.job(id).local_query_done {
                    self.process(id);
                }
            }
            Err(error) => {
                let code = error.code;
                log::warn!(target: LOG, "Server error in directory {:?} {code}", self.job(id).current_folder.server);
                if let Some(d) = self.job(id).dir_item.clone()
                    && code >= 403
                {
                    // In case of an HTTP error, we ignore that directory
                    {
                        let mut d = d.borrow_mut();
                        d.instruction = Instruction::Ignore;
                        d.error_string = error.message;
                    }
                    self.job_emit_finished(id);
                } else {
                    // Fatal for the root job since it has no SyncFileItem, or for the network errors
                    self.emit_fatal_error(error.message, ErrorCategory::NetworkError);
                }
            }
        }
    }

    /// `startAsyncLocalQuery` (`DiscoverySingleLocalDirectoryJob::run`).
    fn start_async_local_query(&mut self, id: JobId) {
        let local_path = format!("{}{}", self.local_dir, self.job(id).current_folder.local);
        self.currently_active_jobs += 1;
        self.job_mut(id).pending_async_jobs += 1;
        let result = self.discovery_single_local_directory(id, &local_path);
        self.push_posted(Posted::LocalListing { job: id, result });
    }

    /// `DiscoverySingleLocalDirectoryJob::run`. Items for undecodable names
    /// are emitted right away (as upstream emits `itemDiscovered` from the
    /// worker, delivered before `finished`).
    fn discovery_single_local_directory(
        &mut self,
        id: JobId,
        local_path_in: &str,
    ) -> LocalListingResult {
        let mut local_path = local_path_in.to_owned();
        if local_path.ends_with('/') {
            local_path.pop();
        }
        let entries = match filesystem::csync_vio_local_readdir(
            &local_path,
            self.file_system_reliable_permissions,
        ) {
            Ok(e) => e,
            Err(filesystem::OpendirError::AccessDenied) => {
                return LocalListingResult::NonFatalError(
                    "Directory not accessible on client, permission denied".to_owned(),
                );
            }
            Err(filesystem::OpendirError::NotFound) => {
                return LocalListingResult::FatalError(format!(
                    "Directory not found: {local_path}"
                ));
            }
            Err(filesystem::OpendirError::NotADirectory) => {
                return LocalListingResult::Finished(Vec::new());
            }
            Err(filesystem::OpendirError::Other) => {
                return LocalListingResult::FatalError(format!(
                    "Error while opening directory {local_path}"
                ));
            }
        };
        let mut results = Vec::new();
        for dirent in entries {
            if dirent.item_type == ItemType::Skip {
                continue;
            }
            let name = match String::from_utf8(dirent.name.clone()) {
                Ok(n) => n,
                Err(_) => {
                    // childIgnored(true) is applied by the caller through the item.
                    let item = new_item();
                    {
                        let mut i = item.borrow_mut();
                        i.file =
                            format!("{}{}", local_path_in, String::from_utf8_lossy(&dirent.name));
                        i.instruction = Instruction::Ignore;
                        i.status = Status::NormalError;
                        i.error_string = "Filename encoding is not valid".to_owned();
                    }
                    // childIgnored(true)
                    self.job_mut(id).child_ignored = true;
                    self.emit_item_discovered(&item);
                    continue;
                }
            };
            results.push(LocalInfo {
                valid: true,
                name,
                case_clash_conflicting_name: String::new(),
                modtime: dirent.modtime,
                size: dirent.size,
                inode: dirent.inode,
                is_directory: matches!(
                    dirent.item_type,
                    ItemType::Directory | ItemType::VirtualDirectory
                ),
                is_hidden: dirent.is_hidden,
                is_sym_link: dirent.item_type == ItemType::SoftLink,
                is_virtual_file: matches!(
                    dirent.item_type,
                    ItemType::VirtualFile | ItemType::VirtualFileDownload
                ),
                is_metadata_missing: dirent.is_metadata_missing,
                is_permissions_invalid: dirent.is_permissions_invalid,
                item_type: dirent.item_type,
                is_locked: false,
            });
        }
        LocalListingResult::Finished(results)
    }

    /// The handlers connected to the local job in `startAsyncLocalQuery`.
    fn local_listing_finished(&mut self, id: JobId, result: LocalListingResult) {
        if !self.alive(id) {
            self.currently_active_jobs -= 1;
            return;
        }
        match result {
            LocalListingResult::FatalError(msg) => {
                self.currently_active_jobs -= 1;
                self.job_mut(id).pending_async_jobs -= 1;
                self.emit_fatal_error(msg, ErrorCategory::NetworkError);
            }
            LocalListingResult::NonFatalError(msg) => {
                self.currently_active_jobs -= 1;
                self.job_mut(id).pending_async_jobs -= 1;
                if let Some(d) = self.job(id).dir_item.clone() {
                    {
                        let mut d = d.borrow_mut();
                        d.instruction = Instruction::Ignore;
                        d.error_string = msg;
                    }
                    self.job_emit_finished(id);
                } else {
                    // Fatal for the root job since it has no SyncFileItem
                    self.emit_fatal_error(msg, ErrorCategory::GenericError);
                }
            }
            LocalListingResult::Finished(results) => {
                self.currently_active_jobs -= 1;
                {
                    let j = self.job_mut(id);
                    j.pending_async_jobs -= 1;
                    j.local_normal_query_entries = results;
                    j.local_query_done = true;
                }
                if self.job(id).server_query_done {
                    self.process(id);
                }
            }
        }
    }

    /// `ProcessDirectoryJob::process`.
    fn process(&mut self, id: JobId) {
        debug_assert!(self.job(id).local_query_done && self.job(id).server_query_done);
        // Build lookup tables for local, remote and db entries.
        let mut entries: BTreeMap<QKey, Entries> = BTreeMap::new();
        let server_entries = std::mem::take(&mut self.job_mut(id).server_normal_query_entries);
        for e in server_entries {
            let k = QKey(e.name.clone());
            entries.entry(k).or_default().server_entry = e;
        }
        // fetch all the name from the DB
        let original = self.job(id).current_folder.original.clone();
        let path_u8 = original.as_bytes().to_vec();
        let mut db_rows = Vec::new();
        if self
            .statedb
            .list_files_in_path(&path_u8, |rec| db_rows.push(rec.clone()))
            .is_err()
        {
            self.db_error();
            return;
        }
        for rec in db_rows {
            let name = if path_u8.is_empty() {
                rec.path_str()
            } else {
                String::from_utf8_lossy(&rec.path[path_u8.len() + 1..]).into_owned()
            };
            entries.entry(QKey(name)).or_default().db_entry = rec;
        }
        let local_entries = std::mem::take(&mut self.job_mut(id).local_normal_query_entries);
        for e in local_entries {
            let k = QKey(e.name.clone());
            entries.entry(k).or_default().local_entry = e;
        }

        let query_server = self.job(id).query_server;
        let current_folder = self.job(id).current_folder.clone();
        for (key, mut e) in entries {
            let name = if e.name_override.is_empty() {
                key.0.clone()
            } else {
                e.name_override.clone()
            };
            let path = current_folder.add_name(&name);
            if !self.list_exclusive_files.is_empty()
                && !self.list_exclusive_files.contains(&path.server)
            {
                continue;
            }
            // If the filename starts with a . we consider it a hidden file
            let is_hidden = e.local_entry.is_hidden || key.0.starts_with('.');
            // E2EE is never set up in this client.
            let is_encrypted_folder_but_e2e_is_not_setup =
                e.server_entry.is_valid() && e.server_entry.is_e2e_encrypted();
            if is_encrypted_folder_but_e2e_is_not_setup {
                self.check_and_update_selective_sync_lists_for_e2ee_folders(&format!(
                    "{}/",
                    path.server
                ));
            }
            let is_blacklisted = query_server == QueryMode::InBlackList
                || self.is_in_selective_sync_black_list(&path.original)
                || is_encrypted_folder_but_e2e_is_not_setup;
            let will_be_excluded =
                self.handle_excluded(id, &path.target, &e, is_hidden, is_blacklisted);
            if will_be_excluded {
                continue;
            }
            if is_blacklisted {
                self.process_blacklisted(id, &path, &e.local_entry, &e.db_entry);
                continue;
            }
            // HACK: make sure the quotes are chopped off the etag.
            e.server_entry.etag = normalize_etag(&e.server_entry.etag);
            self.process_file(id, path, e.local_entry, e.server_entry, e.db_entry);
            if self.fatal.is_some() {
                return;
            }
        }
        self.list_exclusive_files.clear();
        self.post_schedule_more_jobs();
    }

    /// `handleExcluded`: returns true if the file is excluded.
    fn handle_excluded(
        &mut self,
        id: JobId,
        path: &str,
        entries: &Entries,
        is_hidden: bool,
        is_blacklisted: bool,
    ) -> bool {
        let is_directory = entries.local_entry.is_directory || entries.server_entry.is_directory;
        let mut excluded = self.excludes.borrow_mut().traversal_pattern_match(
            path,
            if is_directory {
                ItemType::Directory
            } else {
                ItemType::File
            },
        );
        let file_name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
        let is_local = entries.local_entry.is_valid();
        if excluded == ExcludeType::NotExcluded && file_name.ends_with(' ') {
            excluded = ExcludeType::FileExcludeTrailingSpace;
        }
        // we don't need to trigger a warning if trailing/leading space file is
        // already on the server or has already been synced down
        let was_synced_already = entries.server_entry.is_valid() || entries.db_entry.is_valid();
        let has_leading_or_trailing_spaces = matches!(
            excluded,
            ExcludeType::FileExcludeLeadingSpace
                | ExcludeType::FileExcludeTrailingSpace
                | ExcludeType::FileExcludeLeadingAndTrailingSpace
        );
        let leading_and_trailing_spaces_files_allowed = !self
            .should_enforce_windows_file_name_compatibility
            || self
                .leading_and_trailing_spaces_files_allowed
                .contains(&format!("{}{}", self.local_dir, path));
        if has_leading_or_trailing_spaces
            && (was_synced_already || leading_and_trailing_spaces_files_allowed)
        {
            excluded = ExcludeType::NotExcluded;
        }
        let mut is_invalid_pattern = false;
        if excluded == ExcludeType::NotExcluded
            && !was_synced_already
            && let Some(rx) = &self.invalid_filename_rx
            && !rx.as_str().is_empty()
            && rx.is_match(path)
        {
            excluded = ExcludeType::FileExcludeInvalidChar;
            is_invalid_pattern = true;
        }
        if excluded == ExcludeType::NotExcluded
            && !entries.db_entry.is_valid()
            && self.ignore_hidden_files
            && is_hidden
        {
            excluded = ExcludeType::FileExcludeHidden;
        }
        let local_name = &entries.local_entry.name;
        let split_name: Vec<&str> = local_name.split('.').collect();
        let base_name = split_name.first().copied().unwrap_or("");
        let extension = if split_name.len() > 1 {
            split_name.last().copied().unwrap_or("")
        } else {
            ""
        };
        let caps = self.account.capabilities();
        let forbidden_filenames = caps.forbidden_filenames();
        let forbidden_basenames = caps.forbidden_filename_basenames();
        let forbidden_extensions = caps.forbidden_filename_extensions();
        let forbidden_chars = caps.forbidden_filename_characters();
        let has_forbidden_filename = forbidden_filenames.iter().any(|f| f == local_name);
        let has_forbidden_basename = forbidden_basenames.iter().any(|f| f == base_name);
        let has_forbidden_extension = forbidden_extensions.iter().any(|f| f == extension);
        let mut forbidden_char_match = String::new();
        let contains_forbidden_characters = forbidden_chars.iter().any(|c| {
            if local_name.contains(c.as_str()) {
                forbidden_char_match = c.clone();
                true
            } else {
                false
            }
        });
        if excluded == ExcludeType::NotExcluded
            && !local_name.is_empty()
            && !was_synced_already
            && (self
                .server_blacklisted_files
                .iter()
                .any(|f| f == local_name)
                || has_forbidden_filename
                || has_forbidden_basename
                || has_forbidden_extension
                || contains_forbidden_characters)
        {
            excluded = ExcludeType::FileExcludeServerBlacklisted;
            is_invalid_pattern = true;
        }
        // (The local codec is UTF-8 on the supported platforms.)
        if excluded == ExcludeType::NotExcluded
            && entries.local_entry.is_locked
            && (!entries.db_entry.is_valid()
                || !entries.server_entry.is_valid()
                || entries.server_entry.etag == entries.db_entry.etag)
        {
            excluded = ExcludeType::FileLockedSilentlyExcluded;
            let p = format!("{}{}", self.local_dir, path);
            if let Some(cb) = self.callbacks.seen_locked_file.as_mut() {
                cb(&p);
            }
        }

        if excluded == ExcludeType::NotExcluded && !entries.local_entry.is_sym_link {
            return false;
        } else if matches!(
            excluded,
            ExcludeType::FileSilentlyExcluded | ExcludeType::FileExcludeAndRemove
        ) {
            self.emit_silently_excluded(path);
            return true;
        }

        let item = new_item();
        {
            let mut i = item.borrow_mut();
            i.file = path.to_owned();
            i.original_file = path.to_owned();
            i.instruction = Instruction::Ignore;
        }
        if excluded == ExcludeType::FileExcludeCaseClashConflict
            && self.can_remove_case_clash_conflicted_copy(path, entries)
        {
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Remove;
                i.direction = Direction::Down;
            }
            self.emit_item_discovered(&item);
            return true;
        }

        {
            let mut i = item.borrow_mut();
            if entries.local_entry.is_sym_link {
                // Symbolic links are ignored.
                i.error_string = "Symbolic links are not supported in syncing.".to_owned();
            } else {
                match excluded {
                    ExcludeType::NotExcluded
                    | ExcludeType::FileSilentlyExcluded
                    | ExcludeType::FileExcludeAndRemove => {
                        unreachable!("These were handled earlier")
                    }
                    ExcludeType::FileLockedSilentlyExcluded => {
                        i.error_string = "File is locked by another application.".to_owned();
                    }
                    ExcludeType::FileExcludeList => {
                        i.error_string = "File is listed on the ignore list.".to_owned();
                    }
                    ExcludeType::FileExcludeInvalidChar => {
                        if i.file.ends_with('.') {
                            i.error_string =
                                "File names ending with a period are not supported on this file system."
                                    .to_owned();
                        } else {
                            let invalid = "\\:?*\"<>|".chars().find(|c| i.file.contains(*c));
                            i.error_string = if let Some(c) = invalid {
                                if is_directory {
                                    format!(
                                        "Folder names containing the character \"{c}\" are not supported on this file system."
                                    )
                                } else {
                                    format!(
                                        "File names containing the character \"{c}\" are not supported on this file system."
                                    )
                                }
                            } else if is_invalid_pattern {
                                if is_directory {
                                    "Folder name contains at least one invalid character".to_owned()
                                } else {
                                    "File name contains at least one invalid character".to_owned()
                                }
                            } else if is_directory {
                                "Folder name is a reserved name on this file system.".to_owned()
                            } else {
                                "File name is a reserved name on this file system.".to_owned()
                            };
                        }
                        i.status = Status::FileNameInvalid;
                    }
                    ExcludeType::FileExcludeTrailingSpace
                    | ExcludeType::FileExcludeLeadingSpace
                    | ExcludeType::FileExcludeLeadingAndTrailingSpace => {
                        i.error_string = match excluded {
                            ExcludeType::FileExcludeTrailingSpace => {
                                "Filename contains trailing spaces."
                            }
                            ExcludeType::FileExcludeLeadingSpace => {
                                "Filename contains leading spaces."
                            }
                            _ => "Filename contains leading and trailing spaces.",
                        }
                        .to_owned();
                        i.status = Status::FileNameInvalid;
                        let abs = format!("{}{}", self.local_dir, i.file);
                        if is_local && !self.maybe_rename_for_windows_compatibility(&abs, excluded)
                        {
                            i.error_string.push_str(" Cannot be renamed or uploaded.");
                        }
                    }
                    ExcludeType::FileExcludeLongFilename => {
                        i.error_string = "Filename is too long.".to_owned();
                        i.status = Status::FileNameInvalid;
                    }
                    ExcludeType::FileExcludeHidden => {
                        i.error_string = "File/Folder is ignored because it's hidden.".to_owned();
                    }
                    ExcludeType::FileExcludeStatFailed => {
                        i.error_string = "Stat failed.".to_owned();
                    }
                    ExcludeType::FileExcludeConflict => {
                        i.error_string =
                            "Conflict: Server version downloaded, local copy renamed and not uploaded."
                                .to_owned();
                        i.status = Status::Conflict;
                    }
                    ExcludeType::FileExcludeCaseClashConflict => {
                        i.error_string =
                            "Case Clash Conflict: Server file downloaded and renamed to avoid clash."
                                .to_owned();
                        i.status = Status::FileNameClash;
                    }
                    ExcludeType::FileExcludeCannotEncode => {
                        i.error_string =
                            "The filename cannot be encoded on your file system.".to_owned();
                    }
                    ExcludeType::FileExcludeServerBlacklisted => {
                        let error_string = "The filename is blacklisted on the server.";
                        let mut reason = String::new();
                        if has_forbidden_filename {
                            reason = "Reason: the entire filename is forbidden.".to_owned();
                        }
                        if has_forbidden_basename {
                            reason =
                                "Reason: the filename has a forbidden base name (filename start)."
                                    .to_owned();
                        }
                        if has_forbidden_extension {
                            reason = format!(
                                "Reason: the file has a forbidden extension (.{extension})."
                            );
                        }
                        if contains_forbidden_characters {
                            reason = format!(
                                "Reason: the filename contains a forbidden character ({forbidden_char_match})."
                            );
                        }
                        i.error_string = if reason.is_empty() {
                            error_string.to_owned()
                        } else {
                            format!("{error_string} {reason}")
                        };
                        i.status = Status::FileNameInvalidOnServer;
                        let abs = format!("{}{}", self.local_dir, i.file);
                        if is_local && !self.maybe_rename_for_windows_compatibility(&abs, excluded)
                        {
                            i.error_string.push_str(" Cannot be renamed or uploaded.");
                        }
                    }
                }
            }
        }

        if let Some(d) = self.job(id).dir_item.clone() {
            let st = item.borrow().status;
            let mut d = d.borrow_mut();
            d.is_any_invalid_char_child =
                d.is_any_invalid_char_child || st == Status::FileNameInvalid;
            d.is_any_case_clash_child = d.is_any_case_clash_child || st == Status::FileNameClash;
        }
        self.job_mut(id).child_ignored = true;
        if is_blacklisted {
            self.emit_silently_excluded(path);
        } else {
            self.emit_item_discovered(&item);
        }
        true
    }

    /// `canRemoveCaseClashConflictedCopy`.
    fn can_remove_case_clash_conflicted_copy(&mut self, path: &str, _entries: &Entries) -> bool {
        // Upstream checks the other entries of the directory being processed;
        // case clash conflict copies only arise on case-insensitive file
        // systems, which this port does not target. The record lookup is kept.
        let conflict_record = self.statedb.case_conflict_record_by_path(path);
        let initial = String::from_utf8_lossy(&conflict_record.initial_base_path).into_owned();
        let _original_base_file_name = initial.rsplit('/').next().unwrap_or("").to_owned();
        false
    }

    /// `checkAndUpdateSelectiveSyncListsForE2eeFolders`.
    fn check_and_update_selective_sync_lists_for_e2ee_folders(&mut self, path: &str) {
        let path_with_trailing_slash = nc_journal::utility::trailing_slash_path(path);
        let mut black: Vec<String> = self
            .statedb
            .get_selective_sync_list(SelectiveSyncListType::BlackList)
            .unwrap_or_default();
        if !black.contains(&path_with_trailing_slash) {
            black.push(path_with_trailing_slash.clone());
        }
        black.sort_by(|a, b| qstring_cmp(a, b));
        black.dedup();
        self.statedb
            .set_selective_sync_list(SelectiveSyncListType::BlackList, &black);
        let mut to_remove: Vec<String> = self
            .statedb
            .get_selective_sync_list(SelectiveSyncListType::E2eFoldersToRemoveFromBlacklist)
            .unwrap_or_default();
        if !to_remove.contains(&path_with_trailing_slash) {
            to_remove.push(path_with_trailing_slash);
        }
        to_remove.sort_by(|a, b| qstring_cmp(a, b));
        to_remove.dedup();
        self.statedb.set_selective_sync_list(
            SelectiveSyncListType::E2eFoldersToRemoveFromBlacklist,
            &to_remove,
        );
    }

    /// `processFile`: reconcile local/remote/db information for one item.
    fn process_file(
        &mut self,
        id: JobId,
        path: PathTuple,
        local_entry: LocalInfo,
        server_entry: RemoteInfo,
        db_entry: SyncJournalFileRecord,
    ) {
        let (query_server, query_local) = (self.job(id).query_server, self.job(id).query_local);
        let processing_log = format!(
            "Processing {} | (db/local/remote) | valid: {}/{}/{} | mtime: {}/{}/{} | size: {}/{}/{} | etag: {}//{} | checksum: {}//{} | perm: {}//{} | fileid: {}//{} | inode: {}/{}/ | type: {:?}/{:?}/{:?}",
            path.original,
            db_entry.is_valid(),
            if local_entry.is_valid() {
                "true"
            } else if query_local == QueryMode::ParentNotChanged {
                "db"
            } else {
                "false"
            },
            if server_entry.is_valid() {
                "true"
            } else if query_server == QueryMode::ParentNotChanged {
                "db"
            } else {
                "false"
            },
            db_entry.modtime,
            local_entry.modtime,
            server_entry.modtime,
            db_entry.file_size,
            local_entry.size,
            server_entry.size,
            String::from_utf8_lossy(&db_entry.etag),
            String::from_utf8_lossy(&server_entry.etag),
            String::from_utf8_lossy(&db_entry.checksum_header),
            String::from_utf8_lossy(&server_entry.checksum_header),
            String::from_utf8_lossy(&db_entry.remote_perm.to_db_value()),
            String::from_utf8_lossy(&server_entry.remote_perm.to_db_value()),
            String::from_utf8_lossy(&db_entry.file_id),
            String::from_utf8_lossy(&server_entry.file_id),
            db_entry.inode,
            local_entry.inode,
            db_entry.item_type,
            local_entry.item_type,
            if server_entry.is_directory {
                ItemType::Directory
            } else {
                ItemType::File
            },
        );
        if self.is_renamed(&path.original) {
            log::debug!(target: LOG, "Ignoring renamed");
            return; // Ignore this.
        }
        let item = Rc::new(RefCell::new(SyncFileItem::from_sync_journal_file_record(
            &db_entry,
        )));
        {
            let mut i = item.borrow_mut();
            i.file = path.target.clone();
            i.original_file = path.original.clone();
            i.previous_size = db_entry.file_size;
            i.previous_modtime = db_entry.modtime;
            i.discovery_result = processing_log;
            if db_entry.modtime == local_entry.modtime
                && db_entry.item_type == ItemType::VirtualFile
                && local_entry.item_type == ItemType::File
            {
                i.item_type = ItemType::File;
            }
            // The item shall only have this type if the db request for the virtual download
            // was successful (like: no conflicting remote remove etc).
            if i.item_type == ItemType::VirtualFileDownload {
                i.item_type = ItemType::VirtualFile;
            }
            // Similarly db entries with a dehydration request denote a regular file
            // until the request is processed.
            if i.item_type == ItemType::VirtualFileDehydration {
                i.item_type = ItemType::File;
            }
        }
        if server_entry.is_valid() {
            self.process_file_analyze_remote_info(
                id,
                &item,
                path,
                local_entry,
                server_entry,
                db_entry,
            );
            return;
        }
        // Downloading a virtual file is like a server action and can happen even if
        // server-side nothing has changed
        if query_server == QueryMode::ParentNotChanged
            && db_entry.is_valid()
            && (db_entry.item_type == ItemType::VirtualFileDownload
                || local_entry.item_type == ItemType::VirtualFileDownload)
            && (local_entry.is_valid() || query_local == QueryMode::ParentNotChanged)
        {
            let mut i = item.borrow_mut();
            i.direction = Direction::Down;
            i.instruction = Instruction::Sync;
            i.item_type = ItemType::VirtualFileDownload;
        }
        self.process_file_analyze_local_info(
            id,
            &item,
            path,
            &local_entry,
            &server_entry,
            &db_entry,
            query_server,
        );
    }

    /// `postProcessServerNew`.
    fn post_process_server_new(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        path: PathTuple,
        local_entry: LocalInfo,
        server_entry: RemoteInfo,
        db_entry: SyncJournalFileRecord,
    ) {
        if item.borrow().is_directory() {
            self.job_mut(id).pending_async_jobs += 1;
            let ctx = NewFolderCtx {
                item: item.clone(),
                folder_path: path.server.clone(),
                path,
                local_entry,
                server_entry,
                db_entry,
            };
            // `None`: async, see Completion::NewFolderSize
            if let Some((ctx, blocked)) = self.check_selective_sync_new_folder(id, ctx) {
                self.new_folder_check_done(id, ctx, blocked);
            }
            return;
        }
        // (Virtual files are off: new remote files are always downloaded.)
        let query_server = self.job(id).query_server;
        self.process_file_analyze_local_info(
            id,
            item,
            path,
            &local_entry,
            &server_entry,
            &db_entry,
            query_server,
        );
    }

    /// The callback of `checkSelectiveSyncNewFolder` in `postProcessServerNew`.
    fn new_folder_check_done(&mut self, id: JobId, ctx: NewFolderCtx, blocked: bool) {
        self.job_mut(id).pending_async_jobs -= 1;
        if !blocked {
            let query_server = self.job(id).query_server;
            self.process_file_analyze_local_info(
                id,
                &ctx.item,
                ctx.path,
                &ctx.local_entry,
                &ctx.server_entry,
                &ctx.db_entry,
                query_server,
            );
        }
        self.post_schedule_more_jobs();
    }

    /// `checkSelectiveSyncNewFolder`: `Some(blocked)` when decided right away,
    /// `None` when a PROPFIND for the folder size was started.
    fn check_selective_sync_new_folder(
        &mut self,
        id: JobId,
        ctx: NewFolderCtx,
    ) -> Option<(NewFolderCtx, bool)> {
        let path = ctx.folder_path.clone();
        let remote_perm = ctx.server_entry.remote_perm;
        if self.sync_options.confirm_external_storage
            && remote_perm.has_permission(Permission::IsMounted)
        {
            // external storage. Only allow it if the white list contains
            // exactly this path (not parents)
            if self.selective_sync_white_list.contains(&format!("{path}/")) {
                return Some((ctx, false));
            }
            if let Some(cb) = self.callbacks.new_big_folder.as_mut() {
                cb(&path, true);
            }
            return Some((ctx, true));
        }
        // If this path or the parent is in the white list, then we do not block this file
        if find_path_in_selective_sync_list(&self.selective_sync_white_list, &path) {
            return Some((ctx, false));
        }
        if !self.active_folder_size_limit() {
            // no limit, everything is allowed
            let blocked = self.new_folder_size_checked(&path, false);
            return Some((ctx, blocked));
        }
        // do a PROPFIND to know the size of this folder
        let account = self.account.clone();
        let full = format!("{}{}", self.remote_folder, path);
        let fut = async move {
            let size = folder_size(&account, &full).await;
            Completion::NewFolderSize {
                job: id,
                ctx: Box::new(ctx),
                size,
            }
        };
        self.push_pending(Box::pin(fut));
        None
    }

    /// The `checkFolderSizeLimit` callback of `checkSelectiveSyncNewFolder`.
    /// Returns whether the folder is blocked.
    fn new_folder_size_checked(&mut self, path: &str, big_folder: bool) -> bool {
        if big_folder {
            // we tell the UI there is a new folder
            if let Some(cb) = self.callbacks.new_big_folder.as_mut() {
                cb(path, false);
            }
            return true;
        }
        // it is not too big, put it in the white list (so we will not do more
        // query for the children) and and do not block.
        let sanitised = nc_journal::utility::trailing_slash_path(path);
        let pos = self
            .selective_sync_white_list
            .partition_point(|e| qstring_cmp(e, &sanitised).is_le());
        self.selective_sync_white_list.insert(pos, sanitised);
        false
    }

    /// `checkSelectiveSyncExistingFolder`.
    fn check_selective_sync_existing_folder(&mut self, path: &str) {
        // If no size limit is enforced, or if is in whitelist (explicitly
        // allowed) or in blacklist (explicitly disallowed), do nothing.
        if !self.notify_existing_folder_over_limit()
            || find_path_in_selective_sync_list(&self.selective_sync_white_list, path)
            || find_path_in_selective_sync_list(&self.selective_sync_black_list, path)
        {
            return;
        }
        let account = self.account.clone();
        let full = format!("{}{}", self.remote_folder, path);
        let p = path.to_owned();
        self.push_pending(Box::pin(async move {
            let size = folder_size(&account, &full).await;
            Completion::ExistingFolderSize { path: p, size }
        }));
    }

    /// `processFileAnalyzeRemoteInfo`.
    fn process_file_analyze_remote_info(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        path: PathTuple,
        local_entry: LocalInfo,
        server_entry: RemoteInfo,
        db_entry: SyncJournalFileRecord,
    ) {
        let (query_server, query_local) = (self.job(id).query_server, self.job(id).query_local);
        {
            let mut i = item.borrow_mut();
            i.checksum_header = server_entry.checksum_header.clone();
            i.file_id = server_entry.file_id.clone();
            i.remote_perm = server_entry.remote_perm;
            i.is_shared = server_entry
                .remote_perm
                .has_permission(Permission::IsShared)
                || server_entry.shared_by_me;
            i.shared_by_me = server_entry.shared_by_me;
            i.last_share_state_fetched_timestamp = current_msecs_since_epoch();
            i.item_type = if server_entry.is_directory {
                ItemType::Directory
            } else {
                ItemType::File
            };
            i.etag = server_entry.etag.clone();
            i.direct_download_url = server_entry.direct_download_url.clone();
            i.direct_download_cookies = server_entry.direct_download_cookies.clone();
            i.e2e_encryption_status = if server_entry.is_e2e_encrypted() {
                ItemEncryptionStatus::EncryptedMigratedV2_0
            } else {
                ItemEncryptionStatus::NotEncrypted
            };
            if server_entry.is_e2e_encrypted() {
                i.e2e_encryption_server_capability = from_end_to_end_encryption_api_version(
                    self.account.capabilities().client_side_encryption_version(),
                );
            }
            i.encrypted_file_name = if i.e2e_encryption_status == ItemEncryptionStatus::NotEncrypted
                || server_entry.e2e_mangled_name.is_empty()
            {
                String::new()
            } else {
                let root_path = &self.remote_folder[1..];
                server_entry
                    .e2e_mangled_name
                    .strip_prefix(root_path)
                    .unwrap_or(&server_entry.e2e_mangled_name)
                    .to_owned()
            };
            i.locked = server_entry.locked;
            i.lock_owner_display_name = server_entry.lock_owner_display_name.clone();
            i.lock_owner_id = server_entry.lock_owner_id.clone();
            i.lock_owner_type = server_entry.lock_owner_type;
            i.lock_editor_app = server_entry.lock_editor_app.clone();
            i.lock_time = server_entry.lock_time;
            i.lock_timeout = server_entry.lock_timeout;
            i.lock_token = server_entry.lock_token.clone();
            i.is_live_photo = server_entry.is_live_photo;
            i.live_photo_file = server_entry.live_photo_file.clone();
            i.folder_quota = server_entry.folder_quota;
        }

        // Check for missing server data
        if server_entry.size == -1
            || server_entry.remote_perm.is_null()
            || server_entry.etag.is_empty()
            || server_entry.file_id.is_empty()
        {
            log::warn!(target: LOG, "The response provided by the server is missing data: {:?}", server_entry.name);
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Error;
                i.error_string = if server_entry.is_directory {
                    "Folder is not accessible on the server.".to_owned()
                } else {
                    "File is not accessible on the server.".to_owned()
                };
            }
            self.job_mut(id).child_ignored = true;
            self.emit_item_discovered(item);
            return;
        }

        // We want to check the lock state of this file after the lock time has expired
        if server_entry.locked == LockStatus::LockedItem && server_entry.lock_timeout > 0 {
            let lock_expiration_time = server_entry.lock_time + server_entry.lock_timeout;
            let time_remaining = lock_expiration_time - crate::utility::current_secs_since_epoch();
            // Add on a second as a precaution
            let lock_expiration_timeout = (time_remaining + 1).max(5);
            self.another_sync_needed = true;
            self.files_needing_scheduled_sync
                .insert(path.original.clone(), lock_expiration_timeout);
        } else if server_entry.locked == LockStatus::UnlockedItem && db_entry.lockstate.locked {
            // We have received data that this file has been unlocked remotely
            self.files_unschedule_sync.push(path.original.clone());
        }

        // The file is known in the db already
        if db_entry.is_valid() {
            let size_on_server = server_entry.size;
            if server_entry.is_directory {
                // Even if over quota, continue syncing as normal for now
                self.check_selective_sync_existing_folder(&path.server);
            }
            if server_entry.is_directory != db_entry.is_directory() {
                // If the type of the entity changed, it's like NEW, but
                // needs to delete the other entity first.
                let mut i = item.borrow_mut();
                i.instruction = Instruction::TypeChange;
                i.direction = Direction::Down;
                i.modtime = server_entry.modtime;
                i.size = size_on_server;
            } else if (db_entry.item_type == ItemType::VirtualFileDownload
                || local_entry.item_type == ItemType::VirtualFileDownload)
                && (local_entry.is_valid() || query_local == QueryMode::ParentNotChanged)
            {
                let mut i = item.borrow_mut();
                i.direction = Direction::Down;
                i.instruction = Instruction::Sync;
                i.item_type = ItemType::VirtualFileDownload;
            } else if server_entry.is_valid()
                && !server_entry.remote_perm.is_null()
                && !server_entry.remote_perm.has_permission(Permission::CanRead)
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Remove;
                i.direction = Direction::Down;
            } else if db_entry.etag != server_entry.etag {
                let mut i = item.borrow_mut();
                i.direction = Direction::Down;
                i.modtime = server_entry.modtime;
                i.size = size_on_server;
                if server_entry.is_directory {
                    debug_assert!(db_entry.is_directory());
                    i.instruction = Instruction::UpdateMetadata;
                } else if !local_entry.is_valid() && query_local != QueryMode::ParentNotChanged {
                    // Deleted locally, changed on server
                    i.instruction = Instruction::New;
                } else {
                    i.instruction = Instruction::Sync;
                }
            } else if db_entry.modtime != server_entry.modtime
                && local_entry.size == server_entry.size
                && db_entry.file_size == server_entry.size
                && db_entry.etag == server_entry.etag
            {
                let mut i = item.borrow_mut();
                i.direction = Direction::Down;
                i.modtime = server_entry.modtime;
                i.size = size_on_server;
                i.instruction = Instruction::UpdateMetadata;
            } else if db_entry.remote_perm != server_entry.remote_perm
                || db_entry.file_id != server_entry.file_id
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::UpdateMetadata;
                i.direction = Direction::Down;
            } else {
                // (The E2EE + VFS migration branch is dead code without VFS.)
                self.process_file_analyze_local_info(
                    id,
                    item,
                    path,
                    &local_entry,
                    &server_entry,
                    &db_entry,
                    QueryMode::ParentNotChanged,
                );
                return;
            }
            self.process_file_analyze_local_info(
                id,
                item,
                path,
                &local_entry,
                &server_entry,
                &db_entry,
                query_server,
            );
            return;
        }

        // Unknown in db: new file on the server
        debug_assert!(!db_entry.is_valid());
        if self.check_new_delete_conflict(item) {
            return;
        }
        {
            let mut i = item.borrow_mut();
            i.instruction = Instruction::New;
            i.direction = Direction::Down;
            i.modtime = server_entry.modtime;
            i.size = server_entry.size;
        }
        let conflict_record = if self.no_case_conflict_records_in_db {
            ConflictRecord::default()
        } else {
            let f = item.borrow().file.clone();
            self.statedb.case_conflict_record_by_base_path(&f)
        };
        if conflict_record.is_valid()
            && String::from_utf8_lossy(&conflict_record.path).contains("(case clash from")
        {
            item.borrow_mut().instruction = Instruction::Ignore;
            return;
        }
        if server_entry.is_valid() && self.job(id).is_inside_encrypted_tree && {
            let i = item.borrow();
            !i.is_directory() && !i.is_encrypted()
        } {
            log::warn!(target: LOG, "remote file inside an encrypted folder {}", item.borrow().file);
            item.borrow_mut().instruction = Instruction::Ignore;
            self.emit_item_discovered(item);
            return;
        }
        if server_entry.is_valid()
            && !server_entry.remote_perm.is_null()
            && !server_entry.remote_perm.has_permission(Permission::CanRead)
        {
            item.borrow_mut().instruction = Instruction::Ignore;
            self.emit_item_discovered(item);
            return;
        }
        // Potential NEW/NEW conflict is handled in AnalyzeLocal
        if local_entry.is_valid() {
            self.post_process_server_new(id, item, path, local_entry, server_entry, db_entry);
            return;
        }

        // Not in db or locally: either new or a rename
        // Check for renames (if there is a file with the same file id)
        let mut done = false;
        let mut is_async = false;
        let mut candidates = Vec::new();
        if self
            .statedb
            .get_file_records_by_file_id(&server_entry.file_id, |r| candidates.push(r.clone()))
            .is_err()
        {
            self.db_error();
            return;
        }
        for base in candidates {
            if done {
                break;
            }
            if !base.is_valid() {
                continue;
            }
            // Remote rename of a virtual file we have locally scheduled for download.
            if base.item_type == ItemType::VirtualFileDownload {
                item.borrow_mut().item_type = ItemType::VirtualFileDownload;
                done = true;
                continue;
            }
            // Remote rename targets a file that shall be locally dehydrated.
            if base.item_type == ItemType::VirtualFileDehydration {
                done = true;
                continue;
            }
            // Some things prohibit rename detection entirely.
            if base.is_directory() != item.borrow().is_directory() {
                log::info!(target: LOG, "file types different, not a rename");
                done = true;
                continue;
            }
            if !server_entry.is_directory
                && (base.modtime != server_entry.modtime || base.file_size != server_entry.size)
            {
                log::info!(target: LOG, "file metadata different, forcing a download of the new file");
                done = true;
                continue;
            }
            // Now we know there is a sane rename candidate.
            let original_path = base.path_str();
            if self.is_renamed(&original_path) {
                log::info!(target: LOG, "folder already has a rename entry, skipping");
                continue;
            }
            // A remote rename can also mean Encryption Mangled Name.
            if !base.e2e_mangled_name.is_empty() {
                log::warn!(target: LOG, "Encrypted file can not rename");
                done = true;
                continue;
            }
            let original_path_adjusted = self.adjust_renamed_path(&original_path, Direction::Up);
            if !base.is_directory() {
                let Some(buf) = filesystem::csync_vio_local_stat(
                    &format!("{}{}", self.local_dir, original_path_adjusted),
                    true,
                ) else {
                    log::info!(target: LOG, "Local file does not exist anymore. {original_path_adjusted}");
                    continue;
                };
                if buf.modtime != base.modtime
                    || buf.size != base.file_size
                    || buf.item_type == ItemType::Directory
                {
                    log::info!(target: LOG, "File has changed locally, not a rename. {original_path}");
                    continue;
                }
            } else if !filesystem::is_dir(&format!("{}{}", self.local_dir, original_path_adjusted))
            {
                log::info!(target: LOG, "Local directory does not exist anymore. {original_path_adjusted}");
                continue;
            }
            // Renames of virtuals are possible
            if base.is_virtual_file() {
                item.borrow_mut().item_type = ItemType::VirtualFile;
            }
            let was_deleted_on_server = self.find_and_cancel_deleted_job(&original_path).0;
            if was_deleted_on_server {
                let mut p = path.clone();
                self.post_process_rename_down(item, &base, &original_path, &mut p);
                // The path is used by processFileAnalyzeLocalInfo below.
                return self.after_rename_candidates(
                    id,
                    item,
                    p,
                    local_entry,
                    server_entry,
                    db_entry,
                );
            } else {
                // we need to make a request to the server to know that the
                // original file is deleted on the server
                self.job_mut(id).pending_async_jobs += 1;
                let account = self.account.clone();
                let remote = format!("{}{}", self.remote_folder, original_path);
                let ctx = RenameDownCtx {
                    item: item.clone(),
                    path: path.clone(),
                    local_entry: local_entry.clone(),
                    server_entry: server_entry.clone(),
                    db_entry: db_entry.clone(),
                    base: base.clone(),
                    original_path: original_path.clone(),
                };
                self.push_pending(Box::pin(async move {
                    let result =
                        nc_dav::jobs::request_etag(&account, &remote, &JobOptions::default()).await;
                    Completion::RenameEtagDown {
                        job: id,
                        ctx: Box::new(ctx),
                        result,
                    }
                }));
                done = true;
                is_async = true;
            }
        }
        if is_async {
            return; // We went async
        }
        self.after_rename_candidates(id, item, path, local_entry, server_entry, db_entry);
    }

    /// The tail of `processFileAnalyzeRemoteInfo` after the rename candidates.
    fn after_rename_candidates(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        path: PathTuple,
        local_entry: LocalInfo,
        server_entry: RemoteInfo,
        db_entry: SyncJournalFileRecord,
    ) {
        if item.borrow().instruction == Instruction::New {
            self.post_process_server_new(id, item, path, local_entry, server_entry, db_entry);
            return;
        }
        let query_server = self.job(id).query_server;
        self.process_file_analyze_local_info(
            id,
            item,
            path,
            &local_entry,
            &server_entry,
            &db_entry,
            query_server,
        );
    }

    /// `postProcessRename` (down) of `processFileAnalyzeRemoteInfo`.
    fn post_process_rename_down(
        &mut self,
        item: &SyncFileItemPtr,
        base: &SyncJournalFileRecord,
        original_path: &str,
        path: &mut PathTuple,
    ) {
        let adjusted_original_path = self.adjust_renamed_path(original_path, Direction::Up);
        self.renamed_items_remote
            .insert(original_path.to_owned(), path.target.clone());
        let mut i = item.borrow_mut();
        i.modtime = base.modtime;
        i.inode = base.inode;
        i.instruction = Instruction::Rename;
        i.direction = Direction::Down;
        i.rename_target = path.target.clone();
        i.file = adjusted_original_path.clone();
        i.original_file = original_path.to_owned();
        path.original = original_path.to_owned();
        path.local = adjusted_original_path;
        log::info!(target: LOG, "Rename detected (down) {} -> {}", i.file, i.rename_target);
    }

    /// The `RequestEtagJob` handler of `processFileAnalyzeRemoteInfo`.
    fn rename_etag_down_finished(
        &mut self,
        id: JobId,
        ctx: RenameDownCtx,
        etag: Result<Vec<u8>, HttpError>,
    ) {
        self.job_mut(id).pending_async_jobs -= 1;
        self.post_schedule_more_jobs();
        let RenameDownCtx {
            item,
            mut path,
            local_entry,
            server_entry,
            db_entry,
            base,
            original_path,
        } = ctx;
        let not_found = matches!(&etag, Err(e) if e.code == 404);
        if !not_found || self.is_renamed(&original_path) {
            // If the file exist or if there is another error, consider it is a new file.
            self.post_process_server_new(id, &item, path, local_entry, server_entry, db_entry);
            return;
        }
        // The file do not exist, it is a rename
        // In case the deleted item was discovered in parallel
        self.find_and_cancel_deleted_job(&original_path);
        self.post_process_rename_down(&item, &base, &original_path, &mut path);
        let is_dir = item.borrow().is_directory();
        if is_dir
            && server_entry.is_valid()
            && db_entry.is_valid()
            && server_entry.etag == db_entry.etag
            && server_entry.remote_perm == db_entry.remote_perm
        {
            self.job_mut(id).query_server = QueryMode::ParentNotChanged;
        }
        let recurse_local = if item.borrow().instruction == Instruction::Rename {
            QueryMode::NormalQuery
        } else {
            QueryMode::ParentDontExist
        };
        let query_server = self.job(id).query_server;
        self.process_file_finalize(id, &item, path, is_dir, recurse_local, query_server);
    }

    /// `folderBytesAvailable`.
    fn folder_bytes_available(
        &self,
        id: JobId,
        item: &SyncFileItem,
        server_entry_valid: bool,
    ) -> i64 {
        const UNLIMITED_FREE_SPACE: i64 = -3;
        let is_type_change = item.instruction == Instruction::TypeChange;
        let is_update_metadata_or_rename =
            item.instruction != Instruction::Sync && item.instruction != Instruction::New;
        let is_file_download_or_directory = item.direction != Direction::Up || item.is_directory();
        if item.size == 0
            || is_type_change
            || is_file_download_or_directory
            || is_update_metadata_or_rename
        {
            return UNLIMITED_FREE_SPACE;
        }
        let j = self.job(id);
        if server_entry_valid {
            return j.folder_quota.bytes_available;
        }
        let Some(d) = &j.dir_item else {
            return UNLIMITED_FREE_SPACE;
        };
        let d = d.borrow();
        if let Ok(Some(rec)) = self.statedb.get_file_record(d.file.as_bytes())
            && rec.is_valid()
        {
            return rec.folder_quota.bytes_available;
        }
        d.folder_quota.bytes_available
    }

    /// `processFileAnalyzeLocalInfo`.
    #[allow(clippy::too_many_arguments)]
    fn process_file_analyze_local_info(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        path: PathTuple,
        local_entry: &LocalInfo,
        server_entry: &RemoteInfo,
        db_entry: &SyncJournalFileRecord,
        mut recurse_query_server: QueryMode,
    ) {
        let (query_server, query_local) = (self.job(id).query_server, self.job(id).query_local);
        let no_server_entry = (query_server != QueryMode::ParentNotChanged
            && !server_entry.is_valid())
            || (query_server == QueryMode::ParentNotChanged && !db_entry.is_valid());
        if no_server_entry {
            recurse_query_server = QueryMode::ParentDontExist;
        }
        let mut server_modified = matches!(
            item.borrow().instruction,
            Instruction::New | Instruction::Sync | Instruction::Rename | Instruction::TypeChange
        );
        if server_modified && self.job(id).is_inside_encrypted_tree && {
            let i = item.borrow();
            !i.is_directory() && !i.is_encrypted()
        } {
            item.borrow_mut().instruction = Instruction::Ignore;
        }
        let is_type_change = item.borrow().instruction == Instruction::TypeChange;
        {
            let mut i = item.borrow_mut();
            if server_entry.is_valid() {
                i.folder_quota = server_entry.folder_quota;
            } else {
                i.folder_quota = FolderQuota::default();
            }
        }
        // Decay server modifications to UPDATE_METADATA if the local virtual exists
        let has_local_virtual = local_entry.is_virtual_file
            || (query_local == QueryMode::ParentNotChanged && db_entry.is_virtual_file());
        let virtual_file_download = item.borrow().item_type == ItemType::VirtualFileDownload;
        if server_modified && !is_type_change && !virtual_file_download && has_local_virtual {
            let mut i = item.borrow_mut();
            i.instruction = Instruction::UpdateMetadata;
            server_modified = false;
            i.item_type = ItemType::VirtualFile;
        }
        if db_entry.is_virtual_file()
            && (!local_entry.is_valid() || local_entry.is_virtual_file)
            && !virtual_file_download
            && !is_type_change
        {
            item.borrow_mut().item_type = ItemType::VirtualFile;
        }
        self.job_mut(id).child_modified |= server_modified;

        let finalize = |this: &mut Self, path: PathTuple, recurse_query_server: QueryMode| {
            this.analyze_local_finalize(
                id,
                item,
                path,
                local_entry,
                server_entry,
                db_entry,
                recurse_query_server,
            );
        };

        if !local_entry.is_valid() {
            if query_local == QueryMode::ParentNotChanged && db_entry.is_valid() {
                // Not modified locally (ParentNotChanged)
                if no_server_entry {
                    // not on the server: Removed on the server, delete locally
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::Remove;
                    i.direction = Direction::Down;
                } else if db_entry.item_type == ItemType::VirtualFileDehydration {
                    let mut i = item.borrow_mut();
                    i.direction = Direction::Down;
                    i.instruction = Instruction::Sync;
                    i.item_type = ItemType::VirtualFileDehydration;
                }
            } else if no_server_entry {
                // Not locally, not on the server. The entry is stale!
                log::info!(target: LOG, "Stale DB entry (missing local/server entries) path={}", path.original);
                if self
                    .statedb
                    .delete_file_record(&path.original, true)
                    .is_err()
                {
                    self.emit_fatal_error(
                        format!(
                            "Error while deleting file record {} from the database",
                            path.original
                        ),
                        ErrorCategory::GenericError,
                    );
                }
                return;
            } else if db_entry.is_live_photo && is_quicktime_video(&item.borrow().file) {
                // This is a live photo's video file; the server won't allow
                // deletion of this file so we need to *not* propagate the .mov
                // deletion to the server and redownload the file
                let mut i = item.borrow_mut();
                i.direction = Direction::Down;
                i.instruction = Instruction::Sync;
            } else if !server_modified {
                // Removed locally: also remove on the server.
                if !db_entry.server_has_ignored_files {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::Remove;
                    i.direction = Direction::Up;
                }
            }
            finalize(self, path, recurse_query_server);
            return;
        }

        item.borrow_mut().inode = local_entry.inode;

        if db_entry.is_valid() {
            let type_change = local_entry.is_directory != db_entry.is_directory();
            if !type_change && local_entry.is_virtual_file {
                if no_server_entry {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::Remove;
                    i.direction = Direction::Down;
                }
            } else if !type_change
                && ((db_entry.modtime == local_entry.modtime
                    && db_entry.file_size == local_entry.size)
                    || local_entry.is_directory)
            {
                // Local file unchanged.
                if no_server_entry {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::Remove;
                    i.direction = Direction::Down;
                } else if db_entry.item_type == ItemType::VirtualFileDehydration
                    || local_entry.item_type == ItemType::VirtualFileDehydration
                {
                    let mut i = item.borrow_mut();
                    i.direction = Direction::Down;
                    i.instruction = Instruction::Sync;
                    i.item_type = ItemType::VirtualFileDehydration;
                } else if !server_modified
                    && (db_entry.inode != local_entry.inode
                        || (local_entry.is_metadata_missing
                            && item.borrow().item_type == ItemType::File
                            && !filesystem::is_lnk_file(&item.borrow().file)))
                {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::UpdateMetadata;
                    i.direction = Direction::Down;
                }
            } else if server_modified {
                // There's a local change and a server change: Conflict!
                self.process_file_conflict(item, &path, local_entry, server_entry, db_entry);
            } else if type_change {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::TypeChange;
                i.direction = Direction::Up;
                i.checksum_header.clear();
                i.size = local_entry.size;
                i.modtime = local_entry.modtime;
                i.item_type = if local_entry.is_directory {
                    ItemType::Directory
                } else {
                    ItemType::File
                };
                drop(i);
                self.job_mut(id).child_modified = true;
            } else if db_entry.modtime > 0
                && (local_entry.modtime <= 0 || local_entry.modtime >= 0xFFFF_FFFF)
                && db_entry.file_size == local_entry.size
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::UpdateMetadata;
                i.direction = Direction::Down;
                i.size = if local_entry.size > 0 {
                    local_entry.size
                } else {
                    db_entry.file_size
                };
                i.modtime = db_entry.modtime;
                i.previous_modtime = db_entry.modtime;
                i.item_type = if local_entry.is_directory {
                    ItemType::Directory
                } else {
                    ItemType::File
                };
                drop(i);
                self.job_mut(id).child_modified = true;
            } else if !server_modified
                && !no_server_entry
                && db_entry.file_size == local_entry.size
                && db_entry.modtime == local_entry.modtime
                && db_entry.inode == 0
                && local_entry.inode > 0
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::UpdateMetadata;
                i.direction = Direction::Down;
            } else {
                // Local file was changed
                {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::Sync;
                    if no_server_entry {
                        // Special case! deleted on server, modified on client, the instruction is then NEW
                        i.instruction = Instruction::New;
                    }
                    i.direction = Direction::Up;
                    i.checksum_header.clear();
                    i.size = local_entry.size;
                    i.modtime = local_entry.modtime;
                }
                self.job_mut(id).child_modified = true;
                // Checksum comparison at this stage is only enabled for .eml files,
                // check #4754 #4755
                let is_eml_file = path.original.to_lowercase().ends_with(".eml");
                if is_eml_file
                    && db_entry.file_size == local_entry.size
                    && !db_entry.checksum_header.is_empty()
                    && compute_local_checksum(
                        &db_entry.checksum_header,
                        &format!("{}{}", self.local_dir, path.local),
                        item,
                    )
                    && item.borrow().checksum_header == db_entry.checksum_header
                {
                    log::info!(target: LOG, "NOTE: Checksums are identical, file did not actually change: {}", path.local);
                    item.borrow_mut().instruction = Instruction::UpdateMetadata;
                }
            }
            finalize(self, path, recurse_query_server);
            return;
        }

        debug_assert!(!db_entry.is_valid());
        if local_entry.is_virtual_file && !no_server_entry {
            // Somehow there is a missing DB entry while the virtual file already exists.
            finalize(self, path, recurse_query_server);
            return;
        } else if server_modified {
            self.process_file_conflict(item, &path, local_entry, server_entry, db_entry);
            finalize(self, path, recurse_query_server);
            return;
        }

        if self.check_new_delete_conflict(item) {
            return;
        }

        // New local file or rename
        {
            let mut i = item.borrow_mut();
            i.instruction = Instruction::New;
            i.direction = Direction::Up;
            i.checksum_header.clear();
            i.size = local_entry.size;
            i.modtime = local_entry.modtime;
            i.item_type = if local_entry.is_directory && !local_entry.is_virtual_file {
                ItemType::Directory
            } else if local_entry.is_directory {
                ItemType::VirtualDirectory
            } else if local_entry.is_virtual_file {
                ItemType::VirtualFile
            } else {
                ItemType::File
            };
        }
        self.job_mut(id).child_modified = true;
        if !local_entry.case_clash_conflicting_name.is_empty() {
            item.borrow_mut().instruction = Instruction::Conflict;
        }
        let conflict_record = {
            let f = item.borrow().file.clone();
            self.statedb.case_conflict_record_by_base_path(&f)
        };
        if conflict_record.is_valid()
            && String::from_utf8_lossy(&conflict_record.path).contains("(case clash from")
        {
            item.borrow_mut().instruction = Instruction::Ignore;
            return;
        }

        // Check if it is a move
        let base = match self.statedb.get_file_record_by_inode(local_entry.inode) {
            Ok(b) => b.unwrap_or_default(),
            Err(_) => {
                self.db_error();
                return;
            }
        };
        let original_path = base.path_str();

        // Function to gradually check conditions for accepting a move-candidate
        let is_move = (|| {
            if !base.is_valid() {
                log::info!(target: LOG, "Not a move, no item in db with inode {}", local_entry.inode);
                return false;
            }
            if base.is_directory() != item.borrow().is_directory() {
                log::info!(target: LOG, "Not a move, types don't match");
                return false;
            }
            // Directories and virtual files don't need size/mtime equality
            if !local_entry.is_directory
                && !base.is_virtual_file()
                && (base.modtime != local_entry.modtime || base.file_size != local_entry.size)
            {
                log::info!(target: LOG, "Not a move, mtime or size differs");
                return false;
            }
            // The old file must have been deleted.
            if filesystem::file_exists(&format!("{}{}", self.local_dir, original_path))
                // Exception: If the rename changes case only (like "foo" -> "Foo") the
                // old filename might still point to the same file.
                && !(nc_journal::utility::fs_case_preserving()
                    && original_path.to_lowercase() == path.local.to_lowercase()
                    && original_path != path.local)
            {
                log::info!(target: LOG, "Not a move, base file still exists at {original_path}");
                return false;
            }
            // Verify the checksum where possible
            if !base.checksum_header.is_empty()
                && item.borrow().item_type == ItemType::File
                && base.item_type == ItemType::File
                && compute_local_checksum(
                    &base.checksum_header,
                    &format!("{}{}", self.local_dir, path.original),
                    item,
                )
            {
                log::info!(target: LOG, "checking checksum of potential rename {}", path.original);
                if item.borrow().checksum_header != base.checksum_header {
                    log::info!(target: LOG, "Not a move, checksums differ");
                    return false;
                }
            }
            if self.is_renamed(&original_path) {
                log::info!(target: LOG, "Not a move, base path already renamed");
                return false;
            }
            true
        })();

        let is_stale_virtual_directory =
            local_entry.item_type == ItemType::VirtualDirectory && no_server_entry;
        let is_e2ee_move =
            is_move && (base.is_e2e_encrypted() || self.job(id).is_inside_encrypted_tree);
        if is_e2ee_move {
            self.permanent_deletion_requests
                .insert(original_path.clone());
        }

        // If it's not a move it's just a local-NEW
        if !is_move || is_e2ee_move {
            if base.is_e2e_encrypted() {
                let caps = from_end_to_end_encryption_api_version(
                    self.account.capabilities().client_side_encryption_version(),
                );
                let mut i = item.borrow_mut();
                i.e2e_encryption_status = from_db_encryption_status(base.e2e_encryption_status);
                i.e2e_encryption_server_capability = caps;
                if i.e2e_encryption_status != i.e2e_encryption_server_capability {
                    i.e2e_encryption_status = i.e2e_encryption_server_capability;
                }
            }
            if is_stale_virtual_directory && !is_move {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Remove;
                i.direction = Direction::Down;
                i.item_type = ItemType::VirtualDirectory;
            }
            // (postProcessLocalNew only acts on virtual files.)
            finalize(self, path, recurse_query_server);
            return;
        }

        // Check local permission if we are allowed to put move the file here
        // Technically we should use the permissions from the server, but we'll assume it is the same
        let server_has_mount_root_property = self.account.server_has_mount_root_property();
        let is_external_storage =
            base.remote_perm.has_permission(Permission::IsMounted) && base.is_directory();
        let move_perms = self.check_move_permissions(
            id,
            base.remote_perm,
            &original_path,
            item.borrow().is_directory(),
        );
        if !move_perms.source_ok
            || !move_perms.destination_ok
            || (server_has_mount_root_property && is_external_storage)
        {
            log::info!(target: LOG, "Move without permission to rename base file, source: {}, target: {}, targetNew: {}, isExternalStorage: {is_external_storage}", move_perms.source_ok, move_perms.destination_ok, move_perms.destination_new_ok);
            // If we can create the destination, do that.
            // Permission errors on the destination will be handled by checkPermissions later.
            finalize(self, path, recurse_query_server);

            // If the destination upload will work, we're fine with the source deletion.
            // If the source deletion can't work, checkPermissions will error.
            // In case of external storage mounted folders we are never allowed to move/delete them
            if move_perms.destination_new_ok && !is_external_storage {
                return;
            }
            // The move target is in a read-only folder so it can't be uploaded,
            // but its data is safe at the source (restored below).
            if !local_entry.is_virtual_file
                && item.borrow().instruction == Instruction::Ignore
                && let Some(cb) = self.callbacks.remnant_read_only_folder_discovered.as_mut()
            {
                cb(item);
            }
            // Here we know the new location can't be uploaded: must prevent the source delete.
            // Two cases: either the source item was already processed or not.
            let was_deleted_on_client = self.find_and_cancel_deleted_job(&original_path);
            if was_deleted_on_client.0 {
                // More complicated. The REMOVE is canceled. Restore will happen next sync.
                log::info!(target: LOG, "Undid remove instruction on source {original_path}");
                if self
                    .statedb
                    .delete_file_record(&original_path, true)
                    .is_err()
                {
                    log::warn!(target: LOG, "Failed to delete a file record from the local DB {original_path}");
                }
                self.statedb
                    .schedule_path_for_remote_discovery(original_path.as_bytes());
                self.another_sync_needed = true;
            } else {
                // Signal to future checkPermissions() to forbid the REMOVE and set to restore instead
                log::info!(target: LOG, "Preventing future remove on source {original_path}");
                self.forbidden_deletes
                    .insert(format!("{original_path}/"), true);
            }
            return;
        }

        let was_deleted_on_client = self.find_and_cancel_deleted_job(&original_path);
        if was_deleted_on_client.0 {
            recurse_query_server = if was_deleted_on_client.1 == base.etag {
                QueryMode::ParentNotChanged
            } else {
                QueryMode::NormalQuery
            };
            let mut p = path;
            self.process_rename_up(item, &base, &original_path, &mut p);
            finalize(self, p, recurse_query_server);
        } else {
            // We must query the server to know if the etag has not changed
            self.job_mut(id).pending_async_jobs += 1;
            let server_original_path = format!(
                "{}{}",
                self.remote_folder,
                self.adjust_renamed_path(&original_path, Direction::Down)
            );
            let account = self.account.clone();
            let ctx = RenameUpCtx {
                item: item.clone(),
                path,
                server_entry: server_entry.clone(),
                db_entry: db_entry.clone(),
                base,
                original_path,
                recurse_query_server,
            };
            self.push_pending(Box::pin(async move {
                let result = nc_dav::jobs::request_etag(
                    &account,
                    &server_original_path,
                    &JobOptions::default(),
                )
                .await;
                Completion::RenameEtagUp {
                    job: id,
                    ctx: Box::new(ctx),
                    result,
                }
            }));
        }
    }

    /// `processRename` (up) of `processFileAnalyzeLocalInfo`.
    fn process_rename_up(
        &mut self,
        item: &SyncFileItemPtr,
        base: &SyncJournalFileRecord,
        original_path: &str,
        path: &mut PathTuple,
    ) {
        let adjusted_original_path = self.adjust_renamed_path(original_path, Direction::Down);
        self.renamed_items_local
            .insert(original_path.to_owned(), path.target.clone());
        let mut i = item.borrow_mut();
        i.rename_target = path.target.clone();
        path.server = adjusted_original_path;
        i.file = path.server.clone();
        path.original = original_path.to_owned();
        i.original_file = path.original.clone();
        i.modtime = base.modtime;
        i.inode = base.inode;
        i.instruction = Instruction::Rename;
        i.direction = Direction::Up;
        i.file_id = base.file_id.clone();
        i.remote_perm = base.remote_perm;
        i.is_shared = base.is_shared;
        i.shared_by_me = base.shared_by_me;
        i.last_share_state_fetched_timestamp = base.last_share_state_fetched_timestamp;
        i.etag = base.etag.clone();
        i.item_type = base.item_type;
        // Discard any download/dehydrate tags on the base file.
        if i.item_type == ItemType::VirtualFileDownload {
            i.item_type = ItemType::VirtualFile;
        }
        if i.item_type == ItemType::VirtualFileDehydration {
            i.item_type = ItemType::File;
        }
        log::info!(target: LOG, "Rename detected (up) {} -> {}", i.file, i.rename_target);
    }

    /// The `RequestEtagJob` handler of `processFileAnalyzeLocalInfo`.
    fn rename_etag_up_finished(
        &mut self,
        id: JobId,
        ctx: RenameUpCtx,
        etag: Result<Vec<u8>, HttpError>,
    ) {
        let RenameUpCtx {
            item,
            mut path,
            server_entry,
            db_entry,
            base,
            original_path,
            mut recurse_query_server,
        } = ctx;
        let is_dir = item.borrow().is_directory();
        let cannot_rename = match &etag {
            Err(_) => true,
            Ok(e) => *e != base.etag && !is_dir,
        } || self.is_renamed(&original_path)
            || (self.is_any_parent_being_restored(&original_path)
                && !self.is_rename(id, &original_path));
        if cannot_rename {
            log::info!(target: LOG, "Can't rename because the etag has changed or the directory is gone or we are restoring one of the file's parents. {original_path}");
            // Can't be a rename, leave it as a new. (postProcessLocalNew: virtual files only)
        } else {
            // In case the deleted item was discovered in parallel
            self.find_and_cancel_deleted_job(&original_path);
            self.process_rename_up(&item, &base, &original_path, &mut path);
            recurse_query_server = if etag.as_ref().is_ok_and(|e| *e == base.etag) {
                QueryMode::ParentNotChanged
            } else {
                QueryMode::NormalQuery
            };
        }
        if is_dir
            && server_entry.is_valid()
            && db_entry.is_valid()
            && server_entry.etag == db_entry.etag
            && server_entry.remote_perm == db_entry.remote_perm
        {
            recurse_query_server = QueryMode::ParentNotChanged;
        }
        let is_dir = item.borrow().is_directory();
        self.process_file_finalize(
            id,
            &item,
            path,
            is_dir,
            QueryMode::NormalQuery,
            recurse_query_server,
        );
        self.job_mut(id).pending_async_jobs -= 1;
        self.post_schedule_more_jobs();
    }

    /// The `finalize` lambda of `processFileAnalyzeLocalInfo`.
    #[allow(clippy::too_many_arguments)]
    fn analyze_local_finalize(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        path: PathTuple,
        local_entry: &LocalInfo,
        server_entry: &RemoteInfo,
        db_entry: &SyncJournalFileRecord,
        mut recurse_query_server: QueryMode,
    ) {
        let (query_server, query_local) = (self.job(id).query_server, self.job(id).query_local);
        let mut recurse =
            item.borrow().is_directory() || local_entry.is_directory || server_entry.is_directory;
        // Even if we have a local directory: If the remote is a file that's propagated as a
        // conflict we don't need to recurse into it. (local c1.owncloud, c1/ ; remote: c1)
        {
            let i = item.borrow();
            if i.instruction == Instruction::Conflict && !i.is_directory() {
                recurse = false;
            }
        }
        if query_local != QueryMode::NormalQuery && query_server != QueryMode::NormalQuery {
            recurse = false;
        }
        if local_entry.is_permissions_invalid {
            recurse = true;
        }
        {
            let mut i = item.borrow_mut();
            if (i.direction == Direction::Down
                || i.instruction == Instruction::Conflict
                || i.instruction == Instruction::New
                || i.instruction == Instruction::Sync)
                && i.direction != Direction::Up
                && (i.modtime <= 0 || i.modtime >= 0xFFFF_FFFF)
            {
                i.instruction = Instruction::Error;
                i.error_string = "Cannot sync due to invalid modification time".to_owned();
                i.status = Status::NormalError;
            }
        }
        let folder_quota = self.folder_bytes_available(id, &item.borrow(), server_entry.is_valid());
        if item.borrow().size > folder_quota && folder_quota > -1 {
            let size = item.borrow().size;
            log::info!(target: LOG, "Quota exceeded for item: {} - item bytes used: {size} - folder bytes available: {folder_quota}", item.borrow().file);
            let server_folder = self.job(id).current_folder.server.clone();
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Error;
                i.error_string = if server_folder.is_empty() {
                    format!(
                        "Upload of {} exceeds {} of space left in personal files.",
                        crate::utility::octets_to_string(size),
                        crate::utility::octets_to_string(folder_quota)
                    )
                } else {
                    format!(
                        "Upload of {} exceeds {} of space left in folder {}.",
                        crate::utility::octets_to_string(size),
                        crate::utility::octets_to_string(folder_quota),
                        server_folder
                    )
                };
                i.status = Status::NormalError;
            }
            self.another_sync_needed = true;
            self.files_needing_scheduled_sync.insert(
                path.original.clone(),
                DELAY_INTERVAL_FOR_SYNC_RETRY_FOR_FILES_EXCEED_QUOTA_SECONDS,
            );
        }
        // (queryEditorsKeepingFileBusy only finds editors on Windows.)
        if db_entry.is_valid() && item.borrow().is_directory() {
            let mut i = item.borrow_mut();
            i.e2e_encryption_status = from_db_encryption_status(db_entry.e2e_encryption_status);
            if i.is_encrypted() {
                i.e2e_encryption_server_capability = from_end_to_end_encryption_api_version(
                    self.account.capabilities().client_side_encryption_version(),
                );
            }
        }
        {
            let mut i = item.borrow_mut();
            if local_entry.is_permissions_invalid && i.instruction == Instruction::None {
                i.instruction = Instruction::UpdateMetadata;
                i.direction = Direction::Down;
            }
            i.is_permissions_invalid = local_entry.is_permissions_invalid;
        }
        let recurse_query_local = if query_local == QueryMode::ParentNotChanged {
            QueryMode::ParentNotChanged
        } else if local_entry.is_directory || item.borrow().instruction == Instruction::Rename {
            QueryMode::NormalQuery
        } else {
            QueryMode::ParentDontExist
        };
        if item.borrow().is_directory()
            && server_entry.is_valid()
            && db_entry.is_valid()
            && server_entry.etag == db_entry.etag
            && server_entry.remote_perm == db_entry.remote_perm
        {
            recurse_query_server = QueryMode::ParentNotChanged;
        }
        self.process_file_finalize(
            id,
            item,
            path,
            recurse,
            recurse_query_local,
            recurse_query_server,
        );
    }

    /// `processFileConflict`.
    fn process_file_conflict(
        &mut self,
        item: &SyncFileItemPtr,
        path: &PathTuple,
        local_entry: &LocalInfo,
        server_entry: &RemoteInfo,
        db_entry: &SyncJournalFileRecord,
    ) {
        {
            let mut i = item.borrow_mut();
            i.previous_size = local_entry.size;
            i.previous_modtime = local_entry.modtime;
            if server_entry.is_directory && local_entry.is_directory {
                // Folders of the same path are always considered equals
                i.instruction = Instruction::UpdateMetadata;
                return;
            }
            // A conflict with a virtual should lead to virtual file download
            if db_entry.is_virtual_file() || local_entry.is_virtual_file {
                i.item_type = ItemType::VirtualFileDownload;
            }
            // If there's no content hash, use heuristics
            if server_entry.checksum_header.is_empty() {
                // If the size or mtime is different, it's definitely a conflict.
                let is_conflict = (server_entry.size != local_entry.size)
                    || (server_entry.modtime != local_entry.modtime)
                    || (db_entry.is_valid()
                        && db_entry.modtime != local_entry.modtime
                        && server_entry.modtime == local_entry.modtime);
                // It could be a conflict even if size and mtime match! (see upstream)
                i.instruction = if is_conflict {
                    Instruction::Conflict
                } else {
                    Instruction::UpdateMetadata
                };
                i.direction = if is_conflict {
                    Direction::None
                } else {
                    Direction::Down
                };
                return;
            }
        }
        // Do we have an UploadInfo for this?
        // Maybe the Upload was completed, but the connection was broken just before
        // we received the etag (Issue #5106)
        let up = self.statedb.get_upload_info(&path.original);
        if up.valid && up.content_checksum == server_entry.checksum_header {
            // Solve the conflict into an upload, or nothing
            let item_type = {
                let mut i = item.borrow_mut();
                i.instruction = if up.modtime == local_entry.modtime && up.size == local_entry.size
                {
                    Instruction::None
                } else {
                    Instruction::Sync
                };
                i.direction = Direction::Up;
                i.item_type
            };
            // Update the etag and other server metadata in the journal already
            // (We can't use a typical CSYNC_INSTRUCTION_UPDATE_METADATA because
            // we must not store the size/modtime from the file system)
            if let Ok(rec) = self.statedb.get_file_record(path.original.as_bytes()) {
                let mut rec = rec.unwrap_or_default();
                rec.path = path.original.as_bytes().to_vec();
                rec.etag = server_entry.etag.clone();
                rec.file_id = server_entry.file_id.clone();
                rec.modtime = server_entry.modtime;
                rec.item_type = item_type;
                rec.file_size = server_entry.size;
                rec.remote_perm = server_entry.remote_perm;
                rec.is_shared = server_entry
                    .remote_perm
                    .has_permission(Permission::IsShared)
                    || server_entry.shared_by_me;
                rec.shared_by_me = server_entry.shared_by_me;
                rec.last_share_state_fetched_timestamp = current_msecs_since_epoch();
                rec.checksum_header = server_entry.checksum_header.clone();
                if let Err(e) = self.statedb.set_file_record(&rec) {
                    log::warn!(target: LOG, "Error when setting the file record to the database {} {e}", path.original);
                }
            }
            return;
        }
        // Rely on content hash comparisons to optimize away non-conflicts inside the job
        let mut i = item.borrow_mut();
        i.instruction = Instruction::Conflict;
        i.direction = Direction::None;
    }

    /// `processFileFinalize`.
    fn process_file_finalize(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        path: PathTuple,
        mut recurse: bool,
        recurse_query_local: QueryMode,
        recurse_query_server: QueryMode,
    ) {
        if item.borrow().is_encrypted()
            && !self
                .account
                .capabilities()
                .client_side_encryption_available()
        {
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Ignore;
                i.direction = Direction::None;
            }
            self.emit_item_discovered(item);
            return;
        }
        {
            let mut i = item.borrow_mut();
            // The VFS is off: virtual items become plain files again.
            if i.instruction == Instruction::None
                && !i.is_directory()
                && matches!(
                    i.item_type,
                    ItemType::VirtualFile
                        | ItemType::VirtualFileDownload
                        | ItemType::VirtualFileDehydration
                )
            {
                i.instruction = Instruction::UpdateMetadata;
                i.direction = Direction::Down;
                i.item_type = ItemType::File;
            }
        }
        let rename_like = matches!(
            item.borrow().instruction,
            Instruction::UpdateVfsMetadata | Instruction::UpdateMetadata | Instruction::None
        );
        if path.original != path.target && rename_like {
            let dir_item = self.job(id).dir_item.clone().expect("renamed parent");
            let dir_direction = {
                let d = dir_item.borrow();
                debug_assert!(d.instruction == Instruction::Rename);
                d.direction
            };
            // This is because otherwise subitems are not updated!
            if dir_direction == Direction::Down {
                self.renamed_items_remote
                    .insert(path.original.clone(), path.target.clone());
            } else {
                self.renamed_items_local
                    .insert(path.original.clone(), path.target.clone());
            }
            let mut i = item.borrow_mut();
            i.instruction = Instruction::Rename;
            i.rename_target = path.target.clone();
            i.direction = dir_direction;
        }
        {
            let i = item.borrow();
            if i.instruction != Instruction::None {
                log::info!(target: LOG, "{}", i.discovery_result);
                log::info!(target: LOG, "discovered {} {:?} {:?} {:?}", i.file, i.instruction, i.direction, i.item_type);
            } else {
                log::debug!(target: LOG, "discovered {} {:?} {:?} {:?}", i.file, i.instruction, i.direction, i.item_type);
            }
        }
        {
            let mut i = item.borrow_mut();
            if i.direction == Direction::Up && i.is_encrypted() {
                // (no E2EE: the certificate is never valid)
                i.instruction = Instruction::Error;
                i.error_string =
                    "Cannot modify encrypted item because the selected certificate is not valid."
                        .to_owned();
                i.status = Status::NormalError;
            }
            if i.is_directory() && i.instruction == Instruction::Sync {
                i.instruction = Instruction::UpdateMetadata;
            }
        }
        let removed = item.borrow().instruction == Instruction::Remove;
        if self.check_permissions(id, item) {
            let i = item.borrow();
            if i.is_restoration && i.is_directory() {
                recurse = true;
            }
        } else {
            recurse = false;
        }
        if item.borrow().item_type == ItemType::VirtualDirectory {
            recurse = false;
        }
        if recurse {
            let inside_encrypted =
                self.job(id).is_inside_encrypted_tree || item.borrow().is_encrypted();
            let parent_dir_item = self.job(id).dir_item.clone();
            let last_sync = self.job(id).last_sync_timestamp;
            let job = ProcessDirectoryJob {
                dir_item: Some(item.clone()),
                dir_parent_item: parent_dir_item,
                parent: if removed { None } else { Some(id) },
                last_sync_timestamp: last_sync,
                query_server: recurse_query_server,
                query_local: recurse_query_local,
                server_normal_query_entries: Vec::new(),
                local_normal_query_entries: Vec::new(),
                server_query_done: false,
                local_query_done: false,
                root_permissions: RemotePermissions::null(),
                pending_async_jobs: 0,
                queued_jobs: VecDeque::new(),
                running_jobs: Vec::new(),
                current_folder: path.clone(),
                child_modified: false,
                child_ignored: false,
                is_inside_encrypted_tree: inside_encrypted,
                folder_quota: FolderQuota::default(),
                is_root: false,
                server_job_running: false,
            };
            log::debug!(target: LOG, "PREPARING {:?} {:?} {:?} {:?}", path.server, recurse_query_server, path.local, recurse_query_local);
            self.jobs.push(Some(job));
            let new_id = self.jobs.len() - 1;
            if removed {
                self.deleted_item
                    .insert(path.original.clone(), item.clone());
                self.enqueue_directory_to_delete(&path.original, new_id);
            } else {
                self.job_mut(id).queued_jobs.push_back(new_id);
            }
        } else {
            let i = item.borrow();
            if removed
                // For the purpose of rename deletion, restored deleted placeholder is as if it was deleted
                || (i.item_type == ItemType::VirtualFile && i.instruction == Instruction::New)
            {
                drop(i);
                self.deleted_item
                    .insert(path.original.clone(), item.clone());
            } else {
                drop(i);
            }
            self.emit_item_discovered(item);
        }
    }

    /// `processBlacklisted`.
    fn process_blacklisted(
        &mut self,
        id: JobId,
        path: &PathTuple,
        local_entry: &LocalInfo,
        db_entry: &SyncJournalFileRecord,
    ) {
        if !local_entry.is_valid() {
            return;
        }
        let item = Rc::new(RefCell::new(SyncFileItem::from_sync_journal_file_record(
            db_entry,
        )));
        {
            let mut i = item.borrow_mut();
            i.file = path.target.clone();
            i.original_file = path.original.clone();
            i.inode = local_entry.inode;
            i.is_selective_sync = true;
            if db_entry.is_valid()
                && ((db_entry.modtime == local_entry.modtime
                    && db_entry.file_size == local_entry.size)
                    || (local_entry.is_directory && db_entry.is_directory()))
            {
                i.instruction = Instruction::Remove;
                i.direction = Direction::Down;
            } else {
                i.instruction = Instruction::Ignore;
                i.status = Status::FileIgnored;
                i.error_string =
                    "Ignored because of the \"choose what to sync\" blacklist".to_owned();
            }
        }
        if item.borrow().instruction == Instruction::Ignore {
            self.job_mut(id).child_ignored = true;
        }
        {
            let i = item.borrow();
            log::info!(target: LOG, "Discovered (blacklisted) {} {:?} {:?} {}", i.file, i.instruction, i.direction, i.is_directory());
        }
        let recurse = {
            let i = item.borrow();
            i.is_directory() && i.instruction != Instruction::Ignore
        };
        if recurse {
            let parent_dir_item = self.job(id).dir_item.clone();
            let last_sync = self.job(id).last_sync_timestamp;
            let job = ProcessDirectoryJob {
                dir_item: Some(item.clone()),
                dir_parent_item: parent_dir_item,
                parent: Some(id),
                last_sync_timestamp: last_sync,
                query_server: QueryMode::InBlackList,
                query_local: QueryMode::NormalQuery,
                server_normal_query_entries: Vec::new(),
                local_normal_query_entries: Vec::new(),
                server_query_done: false,
                local_query_done: false,
                root_permissions: RemotePermissions::null(),
                pending_async_jobs: 0,
                queued_jobs: VecDeque::new(),
                running_jobs: Vec::new(),
                current_folder: path.clone(),
                child_modified: false,
                child_ignored: false,
                is_inside_encrypted_tree: false,
                folder_quota: FolderQuota::default(),
                is_root: false,
                server_job_running: false,
            };
            self.jobs.push(Some(job));
            let new_id = self.jobs.len() - 1;
            self.job_mut(id).queued_jobs.push_back(new_id);
        } else {
            self.emit_item_discovered(&item);
        }
    }

    /// `checkPermissions`: if needed, change the item to a restoration item.
    /// Returns false to indicate that this is an error and if it is a
    /// directory, one should not recurse inside it.
    fn check_permissions(&mut self, id: JobId, item: &SyncFileItemPtr) -> bool {
        let (direction, instruction) = {
            let i = item.borrow();
            (i.direction, i.instruction)
        };
        if direction != Direction::Up {
            // Currently we only check server-side permissions
            return true;
        }
        match instruction {
            Instruction::TypeChange | Instruction::New => {
                let j = self.job(id);
                let perms = if !j.root_permissions.is_null() {
                    j.root_permissions
                } else if let Some(d) = &j.dir_item {
                    d.borrow().remote_perm
                } else {
                    j.root_permissions
                };
                let mut i = item.borrow_mut();
                if perms.is_null() {
                    // No permissions set
                    return true;
                } else if i.is_directory()
                    && !perms.has_permission(Permission::CanAddSubDirectories)
                {
                    log::warn!(target: LOG, "checkForPermission: Not allowed because you don't have permission to add subfolders to that folder: {}", i.file);
                    i.instruction = Instruction::Ignore;
                    i.error_string =
                        "Not allowed because you don't have permission to add subfolders to that folder".to_owned();
                    return false;
                } else if !i.is_directory() && !perms.has_permission(Permission::CanAddFile) {
                    log::warn!(target: LOG, "checkForPermission: Not allowed because you don't have permission to add files in that folder: {}", i.file);
                    i.instruction = Instruction::Ignore;
                    i.error_string =
                        "Not allowed because you don't have permission to add files in that folder"
                            .to_owned();
                    return false;
                }
            }
            Instruction::Sync => {
                let mut i = item.borrow_mut();
                let perms = i.remote_perm;
                if perms.is_null() {
                    // No permissions set
                    return true;
                }
                if !perms.has_permission(Permission::CanWrite) {
                    i.instruction = Instruction::Conflict;
                    i.error_string =
                        "Not allowed to upload this file because it is read-only on the server, restoring"
                            .to_owned();
                    i.direction = Direction::Down;
                    i.is_restoration = true;
                    log::warn!(target: LOG, "checkForPermission: RESTORING {} {}", i.file, i.error_string);
                    // Take the things to write to the db from the "other" node (i.e: info from server).
                    let i = &mut *i;
                    std::mem::swap(&mut i.size, &mut i.previous_size);
                    std::mem::swap(&mut i.modtime, &mut i.previous_modtime);
                    return false;
                }
            }
            Instruction::Remove => {
                let file = item.borrow().file.clone();
                let file_slash = format!("{file}/");
                // upperBound then prev: the greatest key <= fileSlash.
                let forbidden = self
                    .forbidden_deletes
                    .range::<String, _>(..=file_slash.clone())
                    .next_back()
                    .or_else(|| self.forbidden_deletes.iter().next())
                    .map(|(k, _)| k.clone());
                if let Some(key) = forbidden
                    && file_slash.starts_with(&key)
                {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::New;
                    i.direction = Direction::Down;
                    i.is_restoration = true;
                    i.error_string = "Moved to invalid target, restoring".to_owned();
                    log::warn!(target: LOG, "checkForPermission: RESTORING {} {}", i.file, i.error_string);
                    return true; // restore sub items
                }
                let perms = item.borrow().remote_perm;
                if perms.is_null() {
                    // No permissions set
                    return true;
                }
                if !perms.has_permission(Permission::CanDelete)
                    || self.is_any_parent_being_restored(&file)
                {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::New;
                    i.direction = Direction::Down;
                    i.is_restoration = true;
                    i.error_string = "Not allowed to remove, restoring".to_owned();
                    log::warn!(target: LOG, "checkForPermission: RESTORING {} {}", i.file, i.error_string);
                    return true; // (we need to recurse to restore sub items)
                }
            }
            _ => {}
        }
        true
    }

    /// `isAnyParentBeingRestored`.
    fn is_any_parent_being_restored(&self, file: &str) -> bool {
        self.directory_names_to_restore_on_propagation
            .iter()
            .any(|d| file.starts_with(&format!("{d}/")))
    }

    /// `isRename`: true when it is just a rename in the same directory.
    fn is_rename(&self, id: JobId, original_path: &str) -> bool {
        let orig = &self.job(id).current_folder.original;
        original_path.starts_with(orig.as_str())
            && original_path.rfind('/').map_or(-1, |i| i as i64) == orig.len() as i64
    }

    /// `checkMovePermissions`.
    fn check_move_permissions(
        &self,
        id: JobId,
        src_perm: RemotePermissions,
        src_path: &str,
        is_directory: bool,
    ) -> MovePermissionResult {
        let j = self.job(id);
        let dest_perms = if !j.root_permissions.is_null() {
            j.root_permissions
        } else if let Some(d) = &j.dir_item {
            d.borrow().remote_perm
        } else {
            j.root_permissions
        };
        let file_perms = src_perm;
        // true when it is just a rename in the same directory. (not a move)
        let is_rename = self.is_rename(id, src_path);
        // Check if we are allowed to move to the destination.
        let mut destination_ok = true;
        let mut destination_new_ok = true;
        if dest_perms.is_null() {
        } else if (is_directory && !dest_perms.has_permission(Permission::CanAddSubDirectories))
            || (!is_directory && !dest_perms.has_permission(Permission::CanAddFile))
        {
            destination_new_ok = false;
        }
        if !is_rename && !destination_new_ok {
            // no need to check for the destination dir permission for renames
            destination_ok = false;
        }
        // check if we are allowed to move from the source
        let mut source_ok = true;
        if !file_perms.is_null()
            && ((is_rename && !file_perms.has_permission(Permission::CanRename))
                || (!is_rename && !file_perms.has_permission(Permission::CanMove)))
        {
            // We are not allowed to move or rename this file
            source_ok = false;
        }
        MovePermissionResult {
            source_ok,
            destination_ok,
            destination_new_ok,
        }
    }

    /// `subJobFinished`.
    fn sub_job_finished(&mut self, id: JobId, child: JobId) {
        let (child_ignored, child_modified, dir_item) = {
            let c = self.job(child);
            (c.child_ignored, c.child_modified, c.dir_item.clone())
        };
        {
            let j = self.job_mut(id);
            j.child_ignored |= child_ignored;
            j.child_modified |= child_modified;
        }
        if let Some(d) = dir_item {
            self.emit_item_discovered(&d);
        }
        let j = self.job_mut(id);
        let before = j.running_jobs.len();
        j.running_jobs.retain(|r| *r != child);
        debug_assert_eq!(before - j.running_jobs.len(), 1);
        self.jobs[child] = None;
        self.post_schedule_more_jobs();
    }

    /// `processSubJobs`: start up to `nb_jobs`, return the number started;
    /// emits `finished` when done.
    fn process_sub_jobs(&mut self, id: JobId, nb_jobs: i32) -> i32 {
        if !self.alive(id) {
            return 0;
        }
        {
            let j = self.job(id);
            if j.queued_jobs.is_empty() && j.running_jobs.is_empty() && j.pending_async_jobs == 0 {
                self.job_mut(id).pending_async_jobs = -1; // We're finished, we don't want to emit finished again
                let (child_modified, child_ignored, dir_item) = {
                    let j = self.job(id);
                    (j.child_modified, j.child_ignored, j.dir_item.clone())
                };
                if let Some(d) = dir_item {
                    let mut d = d.borrow_mut();
                    if child_modified
                        && d.instruction == Instruction::TypeChange
                        && !d.is_directory()
                    {
                        // Replacing a directory by a file is a conflict, if the directory had modified children
                        d.instruction = Instruction::Conflict;
                        if d.direction == Direction::Up {
                            d.item_type = ItemType::Directory;
                            d.direction = Direction::Down;
                        }
                    }
                    if child_ignored && d.instruction == Instruction::Remove {
                        // Do not remove a directory that has ignored files
                        log::info!(target: LOG, "Child ignored for a folder to remove {} direction {:?}", d.file, d.direction);
                        d.instruction = Instruction::None;
                    }
                }
                self.job_emit_finished(id);
                if !self.alive(id) {
                    return 0;
                }
            }
        }
        let mut started = 0;
        let running = self.job(id).running_jobs.clone();
        for rj in running {
            if !self.alive(id) {
                return started;
            }
            if !self.job(id).running_jobs.contains(&rj) {
                continue;
            }
            started += self.process_sub_jobs(rj, nb_jobs - started);
            if started >= nb_jobs {
                return started;
            }
        }
        while self.alive(id) && started < nb_jobs {
            let Some(f) = self.job_mut(id).queued_jobs.pop_front() else {
                break;
            };
            self.job_mut(id).running_jobs.push(f);
            self.job_start(f);
            started += 1;
        }
        started
    }

    /// `dbError`.
    fn db_error(&mut self) {
        self.emit_fatal_error(
            "Error while reading the database".to_owned(),
            ErrorCategory::GenericError,
        );
    }

    /// `maybeRenameForWindowsCompatibility`.
    fn maybe_rename_for_windows_compatibility(
        &self,
        absolute_file_name: &str,
        reason: ExcludeType,
    ) -> bool {
        let allowed = !self.should_enforce_windows_file_name_compatibility
            || self
                .leading_and_trailing_spaces_files_allowed
                .iter()
                .any(|p| p == absolute_file_name);
        if allowed {
            return true;
        }
        match reason {
            ExcludeType::FileExcludeLeadingAndTrailingSpace
            | ExcludeType::FileExcludeLeadingSpace
            | ExcludeType::FileExcludeTrailingSpace => {
                let (dir, name) = match absolute_file_name.rfind('/') {
                    Some(i) => (&absolute_file_name[..i], &absolute_file_name[i + 1..]),
                    None => ("", absolute_file_name),
                };
                let trimmed = name.trim_end();
                let target = format!("{dir}/{trimmed}");
                filesystem::rename(absolute_file_name, &target).is_ok()
            }
            _ => true,
        }
    }

    /// `checkNewDeleteConflict`.
    fn check_new_delete_conflict(&mut self, item: &SyncFileItemPtr) -> bool {
        let file = item.borrow().file.clone();
        if self.recursive_check_for_deleted_parents(&file) {
            log::warn!(target: LOG, "Removing local file inside a remotely deleted folder {file}");
            {
                let mut i = item.borrow_mut();
                i.instruction = Instruction::Remove;
                i.direction = Direction::Down;
                i.wants_specific_actions = SynchronizationOptions::MoveToClientTrashBin;
            }
            self.emit_item_discovered(item);
            return true;
        }
        false
    }
}

/// `MovePermissionResult`.
struct MovePermissionResult {
    /// whether moving/renaming the source is ok
    source_ok: bool,
    /// whether the destination accepts (always true for renames)
    destination_ok: bool,
    /// whether creating a new file/dir in the destination is ok
    destination_new_ok: bool,
}

/// Whether the MIME type of a file name inherits `video/quicktime`
/// (`QMimeDatabase().mimeTypeForFile(file)`, by extension).
fn is_quicktime_video(file: &str) -> bool {
    let lower = file.to_lowercase();
    [".mov", ".qt", ".moov", ".qtvr"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// `computeLocalChecksum`: computes the checksum of `path` with the type of
/// `header` into `item.checksum_header`. Returns whether it worked.
fn compute_local_checksum(header: &[u8], path: &str, item: &SyncFileItemPtr) -> bool {
    let ty = nc_journal::checksums::parse_checksum_header_type(header);
    if !ty.is_empty()
        && let Some(checksum) = nc_journal::checksums::ComputeChecksum::compute_now_on_file(
            std::path::Path::new(path),
            &ty,
        )
        && !checksum.is_empty()
    {
        item.borrow_mut().checksum_header =
            nc_journal::checksums::make_checksum_header(&ty, &checksum);
        return true;
    }
    false
}

/// The `PropfindJob` of `checkFolderSizeLimit`: the folder size, `None` on error.
async fn folder_size(account: &Account, path: &str) -> Option<i64> {
    match nc_dav::jobs::propfind(
        account,
        path,
        &["resourcetype", "http://owncloud.org/ns:size"],
        &JobOptions::default(),
    )
    .await
    {
        Ok(values) => Some(crate::utility::qstring_to_long_long(
            values.get("size").map_or("", String::as_str),
        )),
        Err(_) => None,
    }
}

/// `DiscoverySingleDirectoryJob`: PROPFIND of a directory turned into
/// [`RemoteInfo`]s, plus the information about the directory itself.
async fn discovery_single_directory(
    account: &Account,
    sub_path: &str,
    is_root: bool,
) -> ServerListing {
    let props = nc_dav::jobs::default_lscol_properties(is_root, account);
    let algorithm = if account.server_has_mount_root_property() {
        MountedPermissionAlgorithm::UseMountRootProperty
    } else {
        MountedPermissionAlgorithm::WildGuessMountedSubProperty
    };
    let mut out = ServerListing {
        result: Ok(Vec::new()),
        data_fingerprint: Vec::new(),
        first_permissions: None,
        first_file_id: None,
        folder_quota: None,
        first_etag: Vec::new(),
    };
    let target = Target::Dav(sub_path.to_owned());
    match nc_dav::jobs::ls_col(account, &target, &props, &JobOptions::default()).await {
        Ok((listing, _reply)) => {
            let mut ignored_first = false;
            let mut results = Vec::new();
            for entry in &listing.entries {
                let map = &entry.properties;
                if !ignored_first {
                    // The first entry is for the folder itself, we should process it differently.
                    ignored_first = true;
                    if let Some(p) = map.get("permissions") {
                        out.first_permissions = Some(RemotePermissions::from_server_string_with(
                            p, algorithm, map,
                        ));
                    }
                    if let Some(fp) = map.get("data-fingerprint") {
                        out.data_fingerprint = fp.as_bytes().to_vec();
                        if out.data_fingerprint.is_empty() {
                            // Placeholder that means that the server supports the feature even if it did not set one.
                            out.data_fingerprint = b"[empty]".to_vec();
                        }
                    }
                    if let Some(fid) = map.get("fileid") {
                        match crate::utility::qstring_to_long_long_ok(fid) {
                            Some(n) => out.first_file_id = Some(n),
                            None => {
                                log::warn!(target: LOG, "conversion to qint64 failed _localFileId={fid}")
                            }
                        }
                    }
                    if map.contains_key("quota-used-bytes")
                        && map.contains_key("quota-available-bytes")
                    {
                        out.folder_quota = Some(FolderQuota {
                            bytes_used: qstring_to_double_i64(&map["quota-used-bytes"])
                                .unwrap_or(-1),
                            bytes_available: qstring_to_double_i64(&map["quota-available-bytes"])
                                .unwrap_or(-1),
                        });
                    }
                } else {
                    let file = &entry.href;
                    let mut result = RemoteInfo {
                        valid: true,
                        name: file[file.rfind('/').map_or(0, |i| i + 1)..].to_owned(),
                        size: -1,
                        ..RemoteInfo::default()
                    };
                    property_map_to_remote_info(map, algorithm, &mut result);
                    if result.is_directory {
                        result.size = 0;
                    }
                    results.push(result);
                }
                if let Some(e) = map.get("getetag")
                    && out.first_etag.is_empty()
                {
                    out.first_etag = nc_dav::xml::parse_etag(e.as_bytes());
                }
            }
            if !ignored_first {
                // This is a sanity check, if we haven't _ignoredFirst then it means we
                // never received any directoryListingIteratedSlot which means somehow
                // the server XML was bogus
                out.result = Err(HttpError {
                    code: 0,
                    message: "Server error: PROPFIND reply is not XML formatted!".to_owned(),
                });
            } else {
                out.result = Ok(results);
            }
        }
        Err(reply) => {
            // lsJobFinishedWithErrorSlot
            let content_type = reply.content_type();
            let invalid_content_type = !content_type.contains("application/xml; charset=utf-8")
                && !content_type.contains("application/xml; charset=\"utf-8\"")
                && !content_type.contains("text/xml; charset=utf-8")
                && !content_type.contains("text/xml; charset=\"utf-8\"");
            let mut error_string = reply.error_string();
            log::warn!(target: LOG, "LSCOL job error {} {} {:?}", error_string, reply.http_status, reply.error);
            if reply.is_ok() && invalid_content_type {
                error_string = "The server returned an unexpected response that couldn’t be read. Please reach out to your server administrator.”".to_owned();
                log::warn!(target: LOG, "Server error: PROPFIND reply is not XML formatted!");
            }
            out.result = Err(HttpError {
                code: reply.http_status,
                message: error_string,
            });
        }
    }
    out
}
