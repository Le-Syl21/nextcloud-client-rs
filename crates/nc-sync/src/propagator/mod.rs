// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/owncloudpropagator.{h,cpp}` and
// `owncloudpropagator_p.h` (nextcloud/desktop v34.0.5).

//! The propagator: turns the sorted list of discovered items into a tree of
//! jobs (`PropagateRootDirectory` / `PropagateDirectory` /
//! `PropagatorCompositeJob` / item jobs) and runs it with upstream's
//! scheduling rules.
//!
//! # Execution model
//!
//! The job tree is an arena ([`JobId`] indices replace the `QObject`
//! pointers) and the scheduling functions (`scheduleSelfOrChild`,
//! `slotSubJobFinished`, `finalize`...) are ported one to one as
//! synchronous methods. Item jobs that only touch the file system run
//! synchronously when they are scheduled, exactly like upstream where
//! `start()` calls `done()` before returning. Item jobs that use the network
//! are futures: they are polled once when started (so they register in
//! `_activeJobList` before the scheduler looks again, as upstream's
//! synchronous `start()` does) and then driven by [`Propagator::run`],
//! which handles one completion at a time. `scheduleNextJob()` (a 3 ms
//! timer upstream) runs once all the completions that are ready have been
//! handled. Queued invocations (`QMetaObject::invokeMethod(...,
//! Qt::QueuedConnection)`) go through a posted-event queue.

pub mod bandwidth;
mod bulk;
mod download;
pub use bandwidth::{BandwidthManager, NetworkLimits};
pub use download::{
    CUSTOM_DECOMPRESSED_SAFETY_CHECK_THRESHOLD, GetFileDevice, GetFileJob, GetFileResult,
    create_download_tmp_file_name,
};
mod local;
pub use local::is_path_inside_deleted_dir;
mod put_multi_file;
mod remote;
mod upload;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::StreamExt;
use futures_util::future::LocalBoxFuture;
use futures_util::stream::FuturesUnordered;
use nc_dav::{Account, NetworkError};
use nc_journal::csync::ItemType;
use nc_journal::journal::{
    ConflictRecord, ErrorCategory as BlacklistCategory, SyncJournalDb,
    SyncJournalErrorBlacklistRecord,
};
use tokio_util::sync::CancellationToken;

use crate::filesystem;
use crate::item::{
    Direction, ErrorCategory, Instruction, Status, SyncFileItem, SyncFileItemPtr,
    SyncFileItemVector, SynchronizationOptions, new_item,
};
use crate::options::SyncOptions;

pub(crate) const LOG: &str = "nextcloud.sync.propagator";

/// `criticalFreeSpaceLimit()`: `OWNCLOUD_CRITICAL_FREE_SPACE_BYTES`, 512 MB by
/// default, bounded by [`free_space_limit`].
pub fn critical_free_space_limit() -> i64 {
    let value = std::env::var("OWNCLOUD_CRITICAL_FREE_SPACE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(512 * 1000 * 1000);
    value.clamp(0, free_space_limit().max(0))
}

/// `freeSpaceLimit()`: `OWNCLOUD_FREE_SPACE_BYTES`, 1 GB by default.
pub fn free_space_limit() -> i64 {
    std::env::var("OWNCLOUD_FREE_SPACE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(1000 * 1000 * 1000)
}

fn min_blacklist_time() -> i64 {
    std::env::var("OWNCLOUD_BLACKLIST_TIME_MIN")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .max(25)
}

fn max_blacklist_time() -> i64 {
    match std::env::var("OWNCLOUD_BLACKLIST_TIME_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        Some(v) if v > 0 => v,
        _ => 24 * 60 * 60,
    }
}

/// `createBlacklistEntry`: a blacklist entry, possibly taking into account
/// an old one.
fn create_blacklist_entry(
    old: &SyncJournalErrorBlacklistRecord,
    item: &SyncFileItem,
) -> SyncJournalErrorBlacklistRecord {
    let min = min_blacklist_time();
    let max = max_blacklist_time().max(min);
    let mut entry = SyncJournalErrorBlacklistRecord {
        file: item.file.clone(),
        error_string: item.error_string.clone(),
        last_try_modtime: item.modtime,
        last_try_etag: item.etag.clone(),
        last_try_time: crate::utility::current_secs_since_epoch(),
        rename_target: item.rename_target.clone(),
        retry_count: old.retry_count + 1,
        request_id: item.request_id.clone(),
        // The factor of 5 feels natural: 25s, 2 min, 10 min, ~1h, ~5h, ~24h
        ignore_duration: old.ignore_duration * 5,
        error_category: BlacklistCategory::Normal,
    };
    if item.http_error_code == 403 {
        log::warn!(target: LOG, "Probably firewall error: {}, blacklisting up to 1h only", item.http_error_code);
        entry.ignore_duration = entry.ignore_duration.min(60 * 60);
    } else if item.http_error_code == 413 || item.http_error_code == 415 {
        log::warn!(target: LOG, "Fatal Error condition {}, maximum blacklist ignore time!", item.http_error_code);
        entry.ignore_duration = max;
    }
    entry.ignore_duration = entry.ignore_duration.clamp(min, max);
    if item.status == Status::SoftError {
        // Track these errors, but don't actively suppress them.
        entry.ignore_duration = 0;
    }
    if item.http_error_code == 507 {
        entry.error_category = BlacklistCategory::InsufficientRemoteStorage;
    }
    entry
}

/// `blacklistUpdate`: updates, creates or removes a blacklist entry for the
/// given item. May adjust the status or the error string.
pub fn blacklist_update(journal: &SyncJournalDb, item: &mut SyncFileItem) {
    let old_entry = journal.error_blacklist_entry(&item.file);
    let may_blacklist = item.error_may_be_blacklisted // explicitly flagged for blacklisting
        || (matches!(item.status, Status::NormalError | Status::SoftError | Status::DetailError)
            && item.http_error_code != 0); // or non-local error
    // No new entry? Possibly remove the old one, then done.
    if !may_blacklist {
        if old_entry.is_valid() {
            journal.wipe_error_blacklist_entry(&item.file);
        }
        return;
    }
    let new_entry = create_blacklist_entry(&old_entry, item);
    journal.set_error_blacklist_entry(&new_entry);
    // Suppress the error if it was and continues to be blacklisted.
    // An ignoreDuration of 0 mean we're tracking the error, but not actively
    // suppressing it.
    if item.has_blacklist_entry && new_entry.ignore_duration > 0 {
        item.status = Status::BlacklistedError;
        log::info!(target: LOG, "blacklisting {} for {}, retry count {}", item.file, new_entry.ignore_duration, new_entry.retry_count);
        return;
    }
    // Some soft errors might become louder on repeat occurrence
    if item.status == Status::SoftError && new_entry.retry_count > 1 {
        log::warn!(target: LOG, "escalating soft error on {} to normal error, {}", item.file, item.http_error_code);
        item.status = Status::NormalError;
    }
}

/// `classifyError`: maps a network error to a `SyncFileItem::Status`.
pub fn classify_error(
    nerror: NetworkError,
    http_code: u16,
    another_sync_needed: Option<&Cell<bool>>,
    error_body: &[u8],
) -> Status {
    debug_assert!(nerror != NetworkError::NoError);
    if nerror == NetworkError::RemoteHostClosedError {
        // Sometimes server bugs lead to a connection close on certain files,
        // that shouldn't bring the rest of the syncing to a halt.
        return Status::NormalError;
    }
    if nerror > NetworkError::NoError && nerror <= NetworkError::UnknownProxyError {
        // network error or proxy error -> fatal
        return Status::FatalError;
    }
    if http_code == 503 {
        // When the server is in maintenance mode, we want to exit the sync immediately
        let body = String::from_utf8_lossy(error_body);
        let probably_maintenance = body.contains(r">Sabre\DAV\Exception\ServiceUnavailable<")
            && !body.contains("Storage is temporarily not available");
        return if probably_maintenance {
            Status::FatalError
        } else {
            Status::NormalError
        };
    }
    if http_code == 412 {
        // "Precondition Failed": happens when the e-tag has changed
        return Status::SoftError;
    }
    if http_code == 423 {
        // "Locked": should be temporary.
        if let Some(a) = another_sync_needed {
            a.set(true);
        }
        return Status::FileLocked;
    }
    Status::NormalError
}

/// `PropagatorJob::errorCategoryFromNetworkError`.
pub fn error_category_from_network_error(e: NetworkError) -> ErrorCategory {
    use NetworkError::*;
    match e {
        NoError => ErrorCategory::NoError,
        TemporaryNetworkFailureError | RemoteHostClosedError => ErrorCategory::NetworkError,
        _ => ErrorCategory::GenericError,
    }
}

/// `getEtagFromReply`: `OC-ETag`, else `ETag`, parsed.
pub fn get_etag_from_reply(reply: &nc_dav::Reply) -> Vec<u8> {
    let oc_etag = nc_dav::xml::parse_etag(reply.raw_header("OC-ETag"));
    let etag = nc_dav::xml::parse_etag(reply.raw_header("ETag"));
    if oc_etag.is_empty() { etag } else { oc_etag }
}

/// `getExceptionFromReply`: the Sabre exception name and message of 400,
/// 403 and 415 replies.
pub fn get_exception_from_reply(reply: &nc_dav::Reply) -> (String, String) {
    if !matches!(reply.http_status, 400 | 403 | 415) {
        return (String::new(), String::new());
    }
    let body = String::from_utf8_lossy(&reply.body);
    let Some(start) = body
        .find("<s:exception>")
        .map(|i| i + "<s:exception>".len())
    else {
        return (String::new(), String::new());
    };
    let Some(end) = body[start..].find('<').map(|i| start + i) else {
        return (String::new(), String::new());
    };
    let name = body[start..end].to_owned();
    let Some(mstart) = body[end..]
        .find("<s:message>")
        .map(|i| end + i + "<s:message>".len())
    else {
        return (name, String::new());
    };
    let Some(mend) = body[mstart..].find('<').map(|i| mstart + i) else {
        return (name, String::new());
    };
    (name, body[mstart..mend].to_owned())
}

/// Index of a job in the propagator arena.
pub type JobId = usize;

/// `PropagatorJob::JobState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    NotYetStarted,
    Running,
    Finished,
}

/// `PropagatorJob::JobParallelism`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parallelism {
    /// Jobs can be run in parallel to this job
    FullParallelism,
    /// No other job shall be started until this one has finished.
    WaitForFinished,
}

/// Who receives a job's `finished(status)` signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinishTarget {
    None,
    /// `PropagatorCompositeJob::slotSubJobFinished`.
    Composite(JobId),
    /// `PropagateDirectory::slotFirstJobFinished`.
    DirectoryFirstJob(JobId),
    /// `PropagateDirectory::slotSubJobsFinished` (or the root's override).
    DirectorySubJobs(JobId),
    /// `PropagateRootDirectory::slotDirDeletionJobsFinished`.
    RootDirDeletion(JobId),
    /// `OwncloudPropagator::emitFinished`.
    Propagator,
}

/// The kind of an item job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Ignore,
    VfsUpdateMetadata,
    LocalRemove,
    LocalMkdir,
    LocalRename,
    RemoteDelete,
    RemoteMkdir,
    RemoteMove,
    Download,
    UploadV1,
    UploadNg,
}

struct ItemJob {
    item: SyncFileItemPtr,
    kind: ItemKind,
    parallelism: Parallelism,
    delete_existing: bool,
    /// `PropagateLocalRemove::_deleteToClientTrashBin`.
    delete_to_client_trash_bin: Vec<String>,
}

struct CompositeJob {
    jobs_to_do: VecDeque<JobId>,
    tasks_to_do: VecDeque<SyncFileItemPtr>,
    running_jobs: Vec<JobId>,
    has_error: Status,
    is_any_case_clash_child: bool,
    is_any_invalid_char_child: bool,
}

struct DirectoryJob {
    item: SyncFileItemPtr,
    first_job: Option<JobId>,
    sub_jobs: JobId,
    /// `PropagateRootDirectory` extras.
    root: Option<RootExtras>,
}

struct RootExtras {
    dir_deletion_jobs: JobId,
    error_status: Status,
}

enum NodeKind {
    Item(ItemJob),
    Composite(CompositeJob),
    Directory(DirectoryJob),
    /// `BulkPropagatorJob`.
    Bulk(bulk::BulkJob),
}

struct Node {
    state: JobState,
    target: FinishTarget,
    associated_composite: Option<JobId>,
    kind: NodeKind,
}

/// What an item job reports when it finishes (`done(status, errorString, category)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Done {
    pub status: Status,
    pub error_string: String,
    pub category: ErrorCategory,
}

impl Done {
    pub fn new(status: Status, error_string: impl Into<String>, category: ErrorCategory) -> Self {
        Self {
            status,
            error_string: error_string.into(),
            category,
        }
    }
    pub fn ok() -> Self {
        Self::new(Status::Success, "", ErrorCategory::NoError)
    }
}

/// The result of an item job: `None` when the job returned without calling
/// `done()` (upstream does that when an abort is already requested).
pub(crate) type Outcome = Option<Done>;

/// What a running future of the propagator delivers.
pub(crate) enum Completion {
    /// An item job finished.
    Item(Outcome),
    /// The `POST` of a bulk upload job finished: the reply and the size of
    /// the request body.
    BulkPut(Box<nc_dav::Reply>, i64),
}

/// Signals of the propagator other than `itemCompleted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropagatorEvent {
    /// `touchedFile(fileName)`.
    TouchedFile(String),
    /// `seenLockedFile(fileName)`.
    SeenLockedFile(String),
    /// `insufficientLocalStorage()`.
    InsufficientLocalStorage,
    /// `insufficientRemoteStorage()`.
    InsufficientRemoteStorage,
    /// `newItem(item)` (a conflict copy to upload): its path.
    NewItem(String),
}

/// State shared between the propagator and the running item jobs (the
/// members of `OwncloudPropagator` jobs reach through `propagator()`).
pub(crate) struct Shared {
    pub journal: Arc<SyncJournalDb>,
    pub account: Arc<Account>,
    /// absolute path to the local directory. ends with '/'
    pub local_dir: String,
    /// remote folder, ends with '/'
    pub remote_folder: String,
    pub options: SyncOptions,
    pub abort_requested: Cell<bool>,
    /// Number of running network requests an asynchronous abort leaves
    /// alone (the final MOVE of a chunked upload, the final PUT of a v1
    /// upload): see [`NonAbortable`].
    non_abortable_jobs: Cell<usize>,
    /// An asynchronous abort was requested while a non-abortable request
    /// was running: upstream's `abortFinished` is then never emitted (the
    /// job's running count never reaches 0), so the propagation only ends
    /// normally or at the abort timeout.
    abort_unfinishable: Cell<bool>,
    /// The 5 s abort timeout fired (`abortTimeout`).
    abort_timed_out: Cell<bool>,
    /// `_activeJobList`: (job, isLikelyFinishedQuickly) entries; a job may
    /// be there several times.
    pub active_job_list: RefCell<Vec<(JobId, bool)>>,
    pub another_sync_needed: Cell<bool>,
    /// Per-folder quota guesses.
    pub folder_quota: RefCell<HashMap<String, i64>>,
    /// The size to use for upload chunks (dynamic).
    pub chunk_size: Cell<i64>,
    /// Map original path (as in the DB) to target final path
    pub renamed_directories: RefCell<BTreeMap<String, String>>,
    /// `_bandwidthManager`, which also holds `_uploadLimit` / `_downloadLimit`.
    pub bandwidth: Arc<bandwidth::BandwidthManager>,
    schedule_requested: Cell<bool>,
    wake: tokio::sync::Notify,
    /// Cancelled by any abort (`QNetworkReply::abort()` on asynchronous abort).
    pub soft_abort: CancellationToken,
    /// Cancelled by a synchronous abort only (the final chunk PUT/MOVE).
    pub hard_abort: CancellationToken,
    /// Tasks appended to composites by running jobs (`createConflict`).
    tree_ops: RefCell<Vec<(JobId, SyncFileItemPtr)>>,
    events: RefCell<Vec<PropagatorEvent>>,
    /// Receiver of the signals other than `itemCompleted`: called when they
    /// are emitted (a direct Qt connection), from inside the running jobs.
    event_sink: RefCell<Option<EventCallback>>,
    /// Receiver of `progress(item, bytes)` (`reportProgress`).
    progress_sink: RefCell<Option<ProgressCallback>>,
    /// `committedDiskSpace()` of the running downloads (bytes still to come).
    pub committed_disk_space: RefCell<HashMap<JobId, i64>>,
}

/// Marks a running request that an asynchronous abort does not abort
/// (`mayAbortJob` returning false in `abortNetworkJobs`), for as long as
/// the guard lives.
pub(crate) struct NonAbortable(Rc<Shared>);

impl NonAbortable {
    pub fn new(shared: &Rc<Shared>) -> Self {
        shared
            .non_abortable_jobs
            .set(shared.non_abortable_jobs.get() + 1);
        Self(shared.clone())
    }
}

impl Drop for NonAbortable {
    fn drop(&mut self) {
        self.0
            .non_abortable_jobs
            .set(self.0.non_abortable_jobs.get().saturating_sub(1));
    }
}

impl Shared {
    /// `OwncloudPropagator::abort()`: requests an asynchronous abort.
    fn request_abort(&self) {
        if self.abort_requested.get() {
            return;
        }
        self.abort_requested.set(true);
        if self.non_abortable_jobs.get() > 0 {
            self.abort_unfinishable.set(true);
        }
        self.soft_abort.cancel();
        self.wake.notify_one();
    }

    /// `scheduleNextJob()`.
    pub fn schedule_next_job(&self) {
        self.schedule_requested.set(true);
        self.wake.notify_one();
    }

    pub fn active_add(&self, id: JobId, quick: bool) {
        self.active_job_list.borrow_mut().push((id, quick));
    }

    /// `_activeJobList.removeOne(this)`.
    pub fn active_remove(&self, id: JobId) {
        let mut l = self.active_job_list.borrow_mut();
        if let Some(pos) = l.iter().position(|(j, _)| *j == id) {
            l.remove(pos);
        }
    }

    /// Emits a signal. Like upstream's direct connections, the receiver
    /// runs right away; a signal emitted by the receiver itself is queued
    /// and delivered by the propagator loop.
    pub fn emit(&self, ev: PropagatorEvent) {
        let sink = self.event_sink.borrow_mut().take();
        match sink {
            Some(mut cb) => {
                cb(&ev);
                let mut slot = self.event_sink.borrow_mut();
                if slot.is_none() {
                    *slot = Some(cb);
                }
            }
            None => self.events.borrow_mut().push(ev),
        }
    }

    /// `reportProgress(item, bytes)`: emits `progress(item, bytes)`.
    pub fn report_progress(&self, item: &SyncFileItem, bytes: i64) {
        let sink = self.progress_sink.borrow_mut().take();
        if let Some(mut cb) = sink {
            cb(item, bytes);
            let mut slot = self.progress_sink.borrow_mut();
            if slot.is_none() {
                *slot = Some(cb);
            }
        }
    }

    /// `fullLocalPath(tmp_file_name)`.
    pub fn full_local_path(&self, file: &str) -> String {
        format!("{}{}", self.local_dir, file)
    }

    /// `fullRemotePath(tmp_file_name)`.
    pub fn full_remote_path(&self, file: &str) -> String {
        format!("{}{}", self.remote_folder, file)
    }

    /// `adjustRenamedPath(original)`.
    pub fn adjust_renamed_path(&self, original: &str) -> String {
        crate::discovery::adjust_renamed_path(&self.renamed_directories.borrow(), original)
    }

    /// `maximumActiveTransferJob()`.
    pub fn maximum_active_transfer_job(&self) -> usize {
        let limits = self.bandwidth.limits();
        if limits.download() != 0 || limits.upload() != 0 || self.options.parallel_network_jobs == 0
        {
            // disable parallelism when there is a network limit.
            return 1;
        }
        (self.options.parallel_network_jobs as f64 / 2.0)
            .ceil()
            .min(3.0) as usize
    }

    /// `hardMaximumActiveJob()`.
    pub fn hard_maximum_active_job(&self) -> usize {
        if self.options.parallel_network_jobs == 0 {
            return 1;
        }
        self.options.parallel_network_jobs as usize
    }

    /// `smallFileSize()`.
    pub fn small_file_size() -> i64 {
        100 * 1024
    }

    /// `localFileNameClash`: only on case-preserving file systems.
    pub fn local_file_name_clash(&self, rel_file: &str) -> bool {
        let file = format!("{}{}", self.local_dir, rel_file);
        if !nc_journal::utility::fs_case_preserving() {
            return false;
        }
        // On Linux, the file system is case sensitive, but this code is useful for testing.
        // Just check that there is no other file with the same name and different casing.
        let dir = filesystem::parent_dir(&file);
        let fname = file.rsplit('/').next().unwrap_or("").to_owned();
        let lower = fname.to_lowercase();
        let list: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.to_lowercase() == lower)
                    .collect()
            })
            .unwrap_or_default();
        if list.len() > 1 || (list.len() == 1 && list[0] != fname) {
            log::warn!(target: LOG, "Detected case clash between {file} and {}", list[0]);
            return true;
        }
        false
    }

    /// `updateMetadata(item)` (`staticUpdateMetadata` with the VFS off).
    pub fn update_metadata(&self, item: &SyncFileItem) -> Result<(), String> {
        let fs_path = format!("{}{}", self.local_dir, item.destination());
        let record = item.to_sync_journal_file_record_with_inode(&fs_path);
        self.journal
            .set_file_record(&record)
            .map_err(|e| e.to_string())
    }

    /// `diskSpaceCheck()` (the committed space of running downloads is not
    /// tracked: only the free space is compared with the limits).
    pub fn disk_space_check(&self, committed: i64) -> DiskSpaceResult {
        let free_bytes = filesystem::free_disk_space(&self.local_dir);
        if free_bytes < 0 {
            return DiskSpaceResult::Ok;
        }
        if free_bytes < critical_free_space_limit() {
            return DiskSpaceResult::Critical;
        }
        if free_bytes - committed < free_space_limit() {
            return DiskSpaceResult::Failure;
        }
        DiskSpaceResult::Ok
    }

    /// `createConflict`: renames the local file away as a conflict copy, sets
    /// up the conflict record and, when conflict files are uploaded, appends
    /// an upload task to `composite`.
    pub fn create_conflict(
        &self,
        item: &SyncFileItemPtr,
        composite: Option<JobId>,
    ) -> Result<(), String> {
        let (file, original_file, previous_modtime, previous_size) = {
            let i = item.borrow();
            (
                i.file.clone(),
                i.original_file.clone(),
                i.previous_modtime,
                i.previous_size,
            )
        };
        let fname = self.full_local_path(&file);
        let conflict_mod_time = filesystem::get_mod_time(&fname);
        if conflict_mod_time <= 0 {
            return Err(format!(
                "Impossible to get modification time for file in conflict {fname}"
            ));
        }
        let upload_conflict_files = self.account.capabilities().upload_conflict_files();
        let conflict_user_name = if upload_conflict_files {
            self.account.dav_display_name()
        } else {
            String::new()
        };
        let conflict_file_name =
            crate::utility::make_conflict_file_name(&file, conflict_mod_time, &conflict_user_name);
        let conflict_file_path = self.full_local_path(&conflict_file_name);
        self.emit(PropagatorEvent::TouchedFile(fname.clone()));
        self.emit(PropagatorEvent::TouchedFile(conflict_file_path.clone()));
        if let Err(rename_error) = filesystem::rename(&fname, &conflict_file_path) {
            // If the rename fails, don't replace it.
            if filesystem::is_file_locked(&fname) {
                self.emit(PropagatorEvent::SeenLockedFile(fname));
            }
            return Err(rename_error);
        }
        log::info!(target: LOG, "Created conflict file {fname} -> {conflict_file_name}");
        // Create a new conflict record. To get the base etag, we need to read it from the db.
        let mut conflict_record = ConflictRecord {
            path: conflict_file_name.as_bytes().to_vec(),
            base_modtime: previous_modtime,
            initial_base_path: file.as_bytes().to_vec(),
            ..ConflictRecord::default()
        };
        if let Ok(Some(base_record)) = self.journal.get_file_record(original_file.as_bytes())
            && base_record.is_valid()
        {
            conflict_record.base_etag = base_record.etag;
            conflict_record.base_file_id = base_record.file_id;
        }
        self.journal.set_conflict_record(&conflict_record);
        // Create a new upload job if the new conflict file should be uploaded
        if upload_conflict_files
            && let Some(composite) = composite
            && !filesystem::is_dir(&conflict_file_path)
        {
            let conflict_item = new_item();
            {
                let mut c = conflict_item.borrow_mut();
                c.file = conflict_file_name.clone();
                c.item_type = ItemType::File;
                c.direction = Direction::Up;
                c.instruction = Instruction::New;
                c.modtime = conflict_mod_time;
                c.size = previous_size;
            }
            self.emit(PropagatorEvent::NewItem(conflict_file_name));
            self.tree_ops.borrow_mut().push((composite, conflict_item));
        }
        // Need a new sync to detect the created copy of the conflicting file
        self.another_sync_needed.set(true);
        Ok(())
    }

    /// `createCaseClashConflict`: only reachable on case-preserving file
    /// systems.
    pub fn create_case_clash_conflict(
        &self,
        item: &SyncFileItemPtr,
        temporary_downloaded_file: &str,
    ) -> Option<String> {
        let (file, rename_target, original_file, previous_modtime, item_type) = {
            let i = item.borrow();
            (
                i.file.clone(),
                i.rename_target.clone(),
                i.original_file.clone(),
                i.previous_modtime,
                i.item_type,
            )
        };
        let filename = if item_type == ItemType::File {
            self.full_local_path(&file)
        } else {
            String::new()
        };
        let conflict_mod_time = filesystem::get_mod_time(&filename);
        if conflict_mod_time <= 0 {
            return Some(format!(
                "Impossible to get modification time for file in conflict {filename}"
            ));
        }
        let conflict_file_name =
            crate::utility::make_case_clash_conflict_file_name(&file, conflict_mod_time);
        let conflict_file_path = self.full_local_path(&conflict_file_name);
        self.emit(PropagatorEvent::TouchedFile(filename.clone()));
        self.emit(PropagatorEvent::TouchedFile(conflict_file_path.clone()));
        if let Err(e) = filesystem::rename(temporary_downloaded_file, &conflict_file_path) {
            if filesystem::is_file_locked(&filename) {
                self.emit(PropagatorEvent::SeenLockedFile(filename));
            }
            return Some(e);
        }
        let base_path = if rename_target.is_empty() {
            file
        } else {
            rename_target
        };
        let mut conflict_record = ConflictRecord {
            path: conflict_file_name.as_bytes().to_vec(),
            base_modtime: previous_modtime,
            initial_base_path: base_path.as_bytes().to_vec(),
            ..ConflictRecord::default()
        };
        if let Ok(Some(base_record)) = self.journal.get_file_record(original_file.as_bytes())
            && base_record.is_valid()
        {
            conflict_record.base_etag = base_record.etag;
            conflict_record.base_file_id = base_record.file_id;
        }
        self.journal.set_case_conflict_record(&conflict_record);
        self.account.report_client_status(
            nc_dav::client_status::ClientStatusReportingStatus::DownloadErrorConflictCaseClash,
        );
        self.another_sync_needed.set(true);
        None
    }
}

/// `DiskSpaceResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskSpaceResult {
    Ok,
    Failure,
    Critical,
}

/// Context handed to an async item job.
pub(crate) struct JobCtx {
    pub shared: Rc<Shared>,
    pub id: JobId,
    pub item: SyncFileItemPtr,
    pub associated_composite: Option<JobId>,
    pub delete_existing: bool,
}

enum PostedEvent {
    /// `PropagatorCompositeJob::finalize` queued from `scheduleSelfOrChild`.
    CompositeFinalize(JobId),
    /// A checksum of a bulk upload file is computed (`ComputeChecksum::done`).
    BulkChecksum(
        JobId,
        SyncFileItemPtr,
        bulk::UploadFileInfo,
        bulk::ChecksumStep,
    ),
}

/// Callbacks of the propagator's signals.
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub struct PropagatorCallbacks {
    /// `itemCompleted(item, category)`.
    pub item_completed: Option<Box<dyn FnMut(&SyncFileItemPtr, ErrorCategory)>>,
    /// The other signals (moved into the shared state when the run
    /// starts, so that the jobs emit them directly).
    pub event: Option<EventCallback>,
}

/// Receiver of the propagator signals other than `itemCompleted`.
pub type EventCallback = Box<dyn FnMut(&PropagatorEvent)>;
/// Receiver of `progress(item, bytes)`.
pub type ProgressCallback = Box<dyn FnMut(&SyncFileItem, i64)>;

/// The propagator (`OwncloudPropagator`).
pub struct Propagator {
    pub(crate) shared: Rc<Shared>,
    nodes: Vec<Node>,
    root: JobId,
    futures: FuturesUnordered<LocalBoxFuture<'static, (JobId, Completion)>>,
    posted: VecDeque<PostedEvent>,
    finished_emitted: Option<Status>,
    pub callbacks: PropagatorCallbacks,
    /// `_delayedTasks`: the uploads left to the bulk upload job.
    delayed_tasks: VecDeque<SyncFileItemPtr>,
    /// `_scheduleDelayedTasks`.
    schedule_delayed_tasks: bool,
    /// `_bulkUploadBlackList`: a reference to the sync engine's set.
    bulk_upload_black_list: Rc<RefCell<HashSet<String>>>,
}

fn poll_once<F: Future + ?Sized + Unpin>(fut: &mut F) -> Poll<F::Output> {
    let mut cx = Context::from_waker(std::task::Waker::noop());
    Pin::new(fut).poll(&mut cx)
}

impl Propagator {
    /// `OwncloudPropagator(account, localDir, remoteFolder, journal)` +
    /// `setSyncOptions`.
    pub fn new(
        account: Arc<Account>,
        local_dir: &str,
        remote_folder: &str,
        journal: Arc<SyncJournalDb>,
        options: SyncOptions,
    ) -> Self {
        let hard_abort = CancellationToken::new();
        let soft_abort = hard_abort.child_token();
        let chunk_size = options.initial_chunk_size;
        let shared = Rc::new(Shared {
            journal,
            account,
            local_dir: nc_journal::utility::trailing_slash_path(local_dir),
            remote_folder: nc_journal::utility::trailing_slash_path(remote_folder),
            options,
            abort_requested: Cell::new(false),
            non_abortable_jobs: Cell::new(0),
            abort_unfinishable: Cell::new(false),
            abort_timed_out: Cell::new(false),
            active_job_list: RefCell::new(Vec::new()),
            another_sync_needed: Cell::new(false),
            folder_quota: RefCell::new(HashMap::new()),
            chunk_size: Cell::new(chunk_size),
            renamed_directories: RefCell::new(BTreeMap::new()),
            bandwidth: Arc::new(bandwidth::BandwidthManager::new(Arc::new(
                NetworkLimits::default(),
            ))),
            schedule_requested: Cell::new(false),
            wake: tokio::sync::Notify::new(),
            soft_abort,
            hard_abort,
            tree_ops: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
            event_sink: RefCell::new(None),
            progress_sink: RefCell::new(None),
            committed_disk_space: RefCell::new(HashMap::new()),
        });
        Self {
            shared,
            nodes: Vec::new(),
            root: 0,
            futures: FuturesUnordered::new(),
            posted: VecDeque::new(),
            finished_emitted: None,
            callbacks: PropagatorCallbacks::default(),
            delayed_tasks: VecDeque::new(),
            schedule_delayed_tasks: false,
            bulk_upload_black_list: Rc::new(RefCell::new(HashSet::new())),
        }
    }

    /// Shares the sync engine's bulk upload blacklist (upstream passes
    /// `SyncEngine::_bulkUploadBlackList` by reference to the constructor),
    /// which outlives the propagator.
    pub fn set_bulk_upload_black_list(&mut self, list: Rc<RefCell<HashSet<String>>>) {
        self.bulk_upload_black_list = list;
    }

    pub fn another_sync_needed(&self) -> bool {
        self.shared.another_sync_needed.get()
    }

    /// Sets `_uploadLimit` / `_downloadLimit` (bytes per second, 0: none).
    /// Like the first, queued `switchingTimerExpired()` of upstream's
    /// bandwidth manager, the limits apply to the jobs started next.
    pub fn set_network_limits(&self, upload: i32, download: i32) {
        self.shared.bandwidth.limits().set(upload, download);
        self.shared.bandwidth.switching_timer_expired();
    }

    /// Makes the propagator follow `limits` (shared with the sync engine,
    /// which may change them during the propagation; the bandwidth manager
    /// picks a change up within its 10 s switching interval).
    pub fn share_network_limits(&self, limits: Arc<NetworkLimits>) {
        self.shared.bandwidth.set_limits(limits);
        self.shared.bandwidth.switching_timer_expired();
    }

    /// `abort()`: an asynchronous abort of all running jobs.
    pub fn abort(&mut self) {
        self.shared.request_abort();
    }

    /// A handle to abort the propagation from a callback.
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            shared: Rc::downgrade(&self.shared),
        }
    }

    fn add_node(&mut self, kind: NodeKind) -> JobId {
        self.nodes.push(Node {
            state: JobState::NotYetStarted,
            target: FinishTarget::None,
            associated_composite: None,
            kind,
        });
        self.nodes.len() - 1
    }

    fn new_composite(&mut self) -> JobId {
        self.add_node(NodeKind::Composite(CompositeJob {
            jobs_to_do: VecDeque::new(),
            tasks_to_do: VecDeque::new(),
            running_jobs: Vec::new(),
            has_error: Status::NoStatus,
            is_any_case_clash_child: false,
            is_any_invalid_char_child: false,
        }))
    }

    fn composite(&self, id: JobId) -> &CompositeJob {
        match &self.nodes[id].kind {
            NodeKind::Composite(c) => c,
            _ => panic!("not a composite job"),
        }
    }

    fn composite_mut(&mut self, id: JobId) -> &mut CompositeJob {
        match &mut self.nodes[id].kind {
            NodeKind::Composite(c) => c,
            _ => panic!("not a composite job"),
        }
    }

    fn directory(&self, id: JobId) -> &DirectoryJob {
        match &self.nodes[id].kind {
            NodeKind::Directory(d) => d,
            _ => panic!("not a directory job"),
        }
    }

    fn directory_mut(&mut self, id: JobId) -> &mut DirectoryJob {
        match &mut self.nodes[id].kind {
            NodeKind::Directory(d) => d,
            _ => panic!("not a directory job"),
        }
    }

    /// `createJob(item)`: `None` for items that need no job.
    fn create_job(&mut self, item: &SyncFileItemPtr) -> Option<JobId> {
        let (instruction, direction, is_dir, size) = {
            let i = item.borrow();
            (i.instruction, i.direction, i.is_directory(), i.size)
        };
        let delete_existing = instruction == Instruction::TypeChange;
        let kind = match instruction {
            Instruction::Remove => {
                if direction == Direction::Down {
                    ItemKind::LocalRemove
                } else {
                    ItemKind::RemoteDelete
                }
            }
            Instruction::New | Instruction::TypeChange | Instruction::Conflict if is_dir => {
                // CONFLICT has _direction == None
                if direction != Direction::Up {
                    ItemKind::LocalMkdir
                } else {
                    ItemKind::RemoteMkdir
                }
            }
            Instruction::New
            | Instruction::TypeChange
            | Instruction::Conflict
            | Instruction::Sync => {
                if direction != Direction::Up {
                    ItemKind::Download
                } else if !delete_existing && self.is_delayed_upload_item(item) {
                    // pushDelayedUploadTask
                    self.delayed_tasks.push_back(item.clone());
                    return None;
                } else if size > self.shared.options.initial_chunk_size
                    && self.shared.account.capabilities().chunking_ng()
                {
                    // Item is above _initialChunkSize, thus will be classified as to be chunked
                    ItemKind::UploadNg
                } else {
                    ItemKind::UploadV1
                }
            }
            Instruction::Rename => {
                if direction == Direction::Up {
                    ItemKind::RemoteMove
                } else {
                    ItemKind::LocalRename
                }
            }
            Instruction::UpdateVfsMetadata => ItemKind::VfsUpdateMetadata,
            // (UPDATE_ENCRYPTION_METADATA needs E2EE.)
            Instruction::UpdateEncryptionMetadata => return None,
            Instruction::Ignore | Instruction::Error => ItemKind::Ignore,
            Instruction::None
            | Instruction::Eval
            | Instruction::EvalRename
            | Instruction::StatError
            | Instruction::UpdateMetadata
            | Instruction::CaseClashConflict => return None,
        };
        let parallelism = if item.borrow().is_encrypted() || self.has_encrypted_ancestor(item) {
            Parallelism::WaitForFinished
        } else {
            Parallelism::FullParallelism
        };
        Some(self.add_node(NodeKind::Item(ItemJob {
            item: item.clone(),
            kind,
            parallelism,
            delete_existing,
            delete_to_client_trash_bin: Vec::new(),
        })))
    }

    /// `PropagateItemJob::hasEncryptedAncestor`.
    fn has_encrypted_ancestor(&self, item: &SyncFileItemPtr) -> bool {
        let file = item.borrow().file.clone();
        matches!(
            self.shared.journal.find_encrypted_ancestor_for_record(&file),
            Ok(Some(rec)) if rec.is_valid() && rec.is_e2e_encrypted()
        )
    }

    /// `PropagatorCompositeJob::appendJob`.
    fn composite_append_job(&mut self, composite: JobId, job: JobId) {
        self.nodes[job].associated_composite = Some(composite);
        self.composite_mut(composite).jobs_to_do.push_back(job);
    }

    /// `PropagateDirectory(propagator, item)`.
    fn new_directory(&mut self, item: SyncFileItemPtr) -> JobId {
        let first_job = self.create_job(&item);
        let sub_jobs = self.new_composite();
        let dir = self.add_node(NodeKind::Directory(DirectoryJob {
            item,
            first_job,
            sub_jobs,
            root: None,
        }));
        if let Some(f) = first_job {
            self.nodes[f].target = FinishTarget::DirectoryFirstJob(dir);
            self.nodes[f].associated_composite = Some(sub_jobs);
        }
        self.nodes[sub_jobs].target = FinishTarget::DirectorySubJobs(dir);
        dir
    }

    /// `PropagateRootDirectory(propagator)`.
    fn new_root_directory(&mut self) -> JobId {
        let root = self.new_directory(new_item());
        let dir_deletion_jobs = self.new_composite();
        self.nodes[dir_deletion_jobs].target = FinishTarget::RootDirDeletion(root);
        self.directory_mut(root).root = Some(RootExtras {
            dir_deletion_jobs,
            error_status: Status::NoStatus,
        });
        root
    }

    /// `adjustDeletedFoldersWithNewChildren`.
    fn adjust_deleted_folders_with_new_children(items: &mut SyncFileItemVector) {
        let n = items.len();
        for idx in (0..n).rev() {
            let (file, is_new_up_dir) = {
                let i = items[idx].borrow();
                (
                    i.file.clone(),
                    i.instruction == Instruction::New
                        && i.direction == Direction::Up
                        && i.is_directory()
                        && i.file != "/",
                )
            };
            if !is_new_up_dir {
                continue;
            }
            // #1 get root folder name for the current item that we need to reupload
            let Some(item_root_folder_name) =
                file.split('/').find(|s| !s.is_empty()).map(str::to_owned)
            else {
                continue;
            };
            // #2 iterate backwards (for optimization) and find the root folder by name
            let Some(item_folder_idx) = (0..=idx)
                .rev()
                .find(|&k| items[k].borrow().file == item_root_folder_name)
            else {
                continue;
            };
            let mut next = item_folder_idx;
            loop {
                {
                    let mut f = items[next].borrow_mut();
                    // #5 make sure every folder in the tree is set to CSYNC_INSTRUCTION_NEW
                    if f.is_directory()
                        && f.instruction == Instruction::Remove
                        && f.direction == Direction::Down
                        && file.starts_with(&format!("{}/", f.file))
                    {
                        log::warn!(target: LOG, "WARNING: New directory to upload {file} is in the removed directories tree {} This should not happen! But, we are going to reupload the entire folder structure.", f.file);
                        f.instruction = Instruction::New;
                        f.direction = Direction::Up;
                    }
                }
                next += 1;
                if next >= n || items[next].borrow().file == file {
                    break;
                }
            }
        }
    }

    /// `OwncloudPropagator::start(items)`: builds the job tree.
    pub fn start(&mut self, mut items: SyncFileItemVector) {
        self.shared.abort_requested.set(false);
        // resetDelayedUploadTasks
        self.schedule_delayed_tasks = false;
        self.delayed_tasks.clear();
        // This builds all the jobs needed for the propagation.
        if let Some(regex) = self.shared.options.file_regex() {
            let mut names: HashSet<String> = HashSet::new();
            for i in &items {
                let f = i.borrow().file.clone();
                if regex.is_match(&f) {
                    let mut r = f.as_str();
                    loop {
                        names.insert(r.to_owned());
                        match r.rfind('/') {
                            Some(idx) if idx > 0 => r = &r[..idx],
                            _ => break,
                        }
                    }
                }
            }
            items.retain(|i| names.contains(&i.borrow().file));
        }
        // process each item that is new and is a directory and make sure every
        // parent in its tree has the instruction NEW instead of REMOVE
        Self::adjust_deleted_folders_with_new_children(&mut items);

        self.root = self.new_root_directory();
        let root = self.root;
        let mut directories: Vec<(String, JobId)> = vec![(String::new(), root)];
        let mut directories_to_remove: Vec<JobId> = Vec::new();
        let mut removed_directory = String::new();
        let mut maybe_conflict_directory = String::new();

        for item in &items {
            let (file, destination, instruction, is_dir, wants) = {
                let i = item.borrow();
                (
                    i.file.clone(),
                    i.destination().to_owned(),
                    i.instruction,
                    i.is_directory(),
                    i.wants_specific_actions,
                )
            };
            if !removed_directory.is_empty() && file.starts_with(&removed_directory) {
                // this is an item in a directory which is going to be removed.
                let del_dir_job = directories_to_remove.first().copied();
                let is_new_directory =
                    is_dir && matches!(instruction, Instruction::New | Instruction::TypeChange);
                if instruction == Instruction::Remove || is_new_directory {
                    // If it is a remove it is already taken care of by the removal of the parent directory
                    if wants == SynchronizationOptions::MoveToClientTrashBin
                        && let Some(d) = del_dir_job
                    {
                        self.will_delete_item_to_client_trash_bin(d, &file);
                    }
                    if let Some(d) = del_dir_job {
                        self.increase_affected_count(d);
                    }
                    continue;
                } else if instruction == Instruction::Ignore {
                    continue;
                } else if instruction == Instruction::Rename {
                    // all is good, the rename will be executed before the directory deletion
                } else {
                    log::warn!(target: LOG, "WARNING:  Job within a removed directory?  This should not happen! {file} {instruction:?}");
                }
            }
            // If a CONFLICT item contains files these can't be processed because
            // the conflict handling is likely to rename the directory.
            if !maybe_conflict_directory.is_empty() {
                if destination.starts_with(&maybe_conflict_directory) {
                    log::info!(target: LOG, "Skipping job inside CONFLICT directory {file} {instruction:?}");
                    item.borrow_mut().instruction = Instruction::None;
                    continue;
                } else {
                    maybe_conflict_directory.clear();
                }
            }
            while !destination.starts_with(&directories.last().expect("root").0) {
                directories.pop();
            }
            if is_dir {
                self.start_directory_propagation(
                    item,
                    &mut directories,
                    &mut directories_to_remove,
                    &mut removed_directory,
                    &items,
                );
            } else {
                let top_dir = directories.last().expect("root").1;
                if !self.directory(top_dir).item.borrow().is_file_drop_detected {
                    self.start_file_propagation(
                        item,
                        &mut directories,
                        &mut directories_to_remove,
                        &mut removed_directory,
                        &mut maybe_conflict_directory,
                    );
                }
            }
        }
        let dir_deletion_jobs = self
            .directory(root)
            .root
            .as_ref()
            .expect("root")
            .dir_deletion_jobs;
        for it in directories_to_remove {
            // appendDirDeletionJob
            self.composite_append_job(dir_deletion_jobs, it);
        }
        self.nodes[root].target = FinishTarget::Propagator;
        self.shared.schedule_next_job();
    }

    /// `PropagateDirectory::willDeleteItemToClientTrashBin`.
    fn will_delete_item_to_client_trash_bin(&mut self, dir: JobId, file: &str) {
        let path = self.shared.full_local_path(file);
        if let Some(f) = self.directory(dir).first_job
            && let NodeKind::Item(j) = &mut self.nodes[f].kind
            && j.kind == ItemKind::LocalRemove
        {
            j.delete_to_client_trash_bin.push(path);
        }
    }

    /// `PropagateDirectory::increaseAffectedCount`.
    fn increase_affected_count(&mut self, job: JobId) {
        let first = match &self.nodes[job].kind {
            NodeKind::Directory(d) => d.first_job,
            _ => None,
        };
        if let Some(f) = first
            && let NodeKind::Item(j) = &self.nodes[f].kind
        {
            j.item.borrow_mut().affected_items += 1;
        }
    }

    /// `startDirectoryPropagation`.
    fn start_directory_propagation(
        &mut self,
        item: &SyncFileItemPtr,
        directories: &mut Vec<(String, JobId)>,
        directories_to_remove: &mut Vec<JobId>,
        removed_directory: &mut String,
        items: &SyncFileItemVector,
    ) {
        let dir_job = self.new_directory(item.clone());
        let (instruction, direction, destination, file) = {
            let i = item.borrow();
            (
                i.instruction,
                i.direction,
                i.destination().to_owned(),
                i.file.clone(),
            )
        };
        if instruction == Instruction::TypeChange && direction == Direction::Up {
            // Skip all potential uploads to the new folder.
            for dir_item in items {
                if dir_item
                    .borrow()
                    .destination()
                    .starts_with(&format!("{destination}/"))
                {
                    dir_item.borrow_mut().instruction = Instruction::None;
                    self.shared.another_sync_needed.set(true);
                }
            }
        }
        if instruction == Instruction::Remove {
            // We do the removal of directories at the end, because there might be moves from
            // these directories that will happen later.
            directories_to_remove.insert(0, dir_job);
            *removed_directory = format!("{file}/");
            // We should not update the etag of parent directories of the removed directory
            // since it would be done before the actual remove (issue #1845)
            for (_, d) in directories.iter() {
                let dir_item = self.directory(*d).item.clone();
                let mut di = dir_item.borrow_mut();
                if di.instruction == Instruction::UpdateMetadata {
                    di.instruction = Instruction::None;
                }
            }
        } else {
            let current_dir_job = directories.last().expect("root").1;
            let sub = self.directory(current_dir_job).sub_jobs;
            self.composite_append_job(sub, dir_job);
        }
        directories.push((format!("{destination}/"), dir_job));
        // (file drop / E2EE metadata migration jobs need E2EE)
    }

    /// `startFilePropagation`.
    fn start_file_propagation(
        &mut self,
        item: &SyncFileItemPtr,
        directories: &mut [(String, JobId)],
        directories_to_remove: &mut Vec<JobId>,
        removed_directory: &mut String,
        maybe_conflict_directory: &mut String,
    ) {
        let (instruction, file) = {
            let i = item.borrow();
            (i.instruction, i.file.clone())
        };
        if instruction == Instruction::TypeChange {
            // will delete directories, so defer execution
            if let Some(job) = self.create_job(item) {
                directories_to_remove.insert(0, job);
            }
            *removed_directory = format!("{file}/");
        } else {
            let top = directories.last().expect("root").1;
            let sub = self.directory(top).sub_jobs;
            self.composite_mut(sub).tasks_to_do.push_back(item.clone());
        }
        if instruction == Instruction::Conflict {
            // This might be a file or a directory on the local side. If it's a
            // directory we want to skip processing items inside it.
            *maybe_conflict_directory = format!("{file}/");
        }
    }

    // ------------------------------------------------------------------
    // Running

    /// Runs the propagation until the root job finished (or the abort
    /// completed). Returns the status of `finished(status)`.
    pub async fn run(&mut self) -> Status {
        if let Some(cb) = self.callbacks.event.take() {
            *self.shared.event_sink.borrow_mut() = Some(cb);
        }
        // The bandwidth manager's timers (started with the propagator upstream).
        let bandwidth = self.shared.bandwidth.clone();
        let bandwidth_timers = bandwidth.run_timers();
        tokio::pin!(bandwidth_timers);
        loop {
            self.drain_events();
            while let Some(ev) = self.posted.pop_front() {
                match ev {
                    PostedEvent::CompositeFinalize(c) => self.composite_finalize(c),
                    PostedEvent::BulkChecksum(id, item, file, step) => {
                        self.bulk_checksum_done(id, item, file, step);
                    }
                }
                self.drain_events();
            }
            if let Some(status) = self.finished_emitted {
                return status;
            }
            // Handle every completion that is ready before scheduling more.
            let mut handled = false;
            while let Some(Some((id, outcome))) =
                futures_util::FutureExt::now_or_never(self.futures.next())
            {
                self.handle_outcome(id, outcome);
                handled = true;
                if self.finished_emitted.is_some() {
                    break;
                }
            }
            if handled {
                continue;
            }
            if self.shared.abort_requested.get()
                && self.futures.is_empty()
                && (!self.shared.abort_unfinishable.get() || self.shared.abort_timed_out.get())
            {
                // `abortFinished` of the root job: every running job is done.
                self.emit_finished(Status::NormalError);
                continue;
            }
            if self.shared.schedule_requested.replace(false) {
                self.schedule_next_job_impl();
                continue;
            }
            if self.futures.is_empty() {
                log::warn!(target: LOG, "Propagation stalled: no running job and nothing scheduled");
                self.emit_finished(Status::NormalError);
                continue;
            }
            let shared = self.shared.clone();
            let abort_timeout = async {
                if shared.abort_requested.get() {
                    // Give asynchronous abort 5000 msec to finish on its own
                    tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                next = self.futures.next() => {
                    if let Some((id, outcome)) = next {
                        self.handle_outcome(id, outcome);
                    }
                }
                _ = shared.wake.notified() => {}
                _ = &mut bandwidth_timers => {}
                _ = abort_timeout => {
                    // abortTimeout: abort synchronously and finish
                    shared.abort_timed_out.set(true);
                    shared.hard_abort.cancel();
                }
            }
        }
    }

    fn drain_events(&mut self) {
        let events: Vec<PropagatorEvent> = std::mem::take(&mut *self.shared.events.borrow_mut());
        for e in events {
            self.shared.emit(e);
        }
    }

    /// Connects `progress(item, bytes)`.
    pub fn set_progress_callback(&mut self, cb: ProgressCallback) {
        *self.shared.progress_sink.borrow_mut() = Some(cb);
    }

    /// Applies the tasks appended by running jobs (`composite->appendTask`).
    fn apply_tree_ops(&mut self) {
        let ops: Vec<(JobId, SyncFileItemPtr)> =
            std::mem::take(&mut *self.shared.tree_ops.borrow_mut());
        for (composite, item) in ops {
            self.composite_mut(composite).tasks_to_do.push_back(item);
        }
    }

    fn handle_outcome(&mut self, id: JobId, completion: Completion) {
        self.apply_tree_ops();
        match completion {
            Completion::Item(Some(done)) => self.item_done(id, done),
            Completion::Item(None) => {}
            Completion::BulkPut(reply, sent) => self.bulk_put_finished(id, *reply, sent),
        }
        self.drain_events();
    }

    /// `emitFinished(status)`: only once.
    fn emit_finished(&mut self, status: Status) {
        if self.finished_emitted.is_none() {
            self.finished_emitted = Some(status);
        }
        self.shared.abort_requested.set(false);
        self.shared.abort_unfinishable.set(false);
        self.shared.abort_timed_out.set(false);
    }

    /// `scheduleNextJobImpl`.
    fn schedule_next_job_impl(&mut self) {
        let active = self.shared.active_job_list.borrow().len();
        let max_transfer = self.shared.maximum_active_transfer_job();
        if active < max_transfer {
            if self.schedule_self_or_child(self.root) {
                self.shared.schedule_next_job();
            }
        } else if active < self.shared.hard_maximum_active_job() {
            // NOTE: Only counts the first 3 jobs! Then for each one that is
            // likely finished quickly, we can launch another one.
            let likely_finished_quickly_count = self
                .shared
                .active_job_list
                .borrow()
                .iter()
                .take(max_transfer)
                .filter(|(_, quick)| *quick)
                .count();
            if active < max_transfer + likely_finished_quickly_count {
                log::debug!(target: LOG, "Can pump in another request! activeJobs = {active}");
                if self.schedule_self_or_child(self.root) {
                    self.shared.schedule_next_job();
                }
            }
        }
    }

    /// `parallelism()` of any job.
    fn parallelism(&self, id: JobId) -> Parallelism {
        match &self.nodes[id].kind {
            NodeKind::Item(j) => j.parallelism,
            NodeKind::Composite(c) => {
                // If any of the running sub jobs is not parallel, we have to wait
                for r in &c.running_jobs {
                    let p = self.parallelism(*r);
                    if p != Parallelism::FullParallelism {
                        return p;
                    }
                }
                Parallelism::FullParallelism
            }
            NodeKind::Bulk(_) => Parallelism::FullParallelism,
            NodeKind::Directory(d) => {
                if d.root.is_some() {
                    // the root directory parallelism isn't important
                    return Parallelism::WaitForFinished;
                }
                if let Some(f) = d.first_job
                    && self.parallelism(f) != Parallelism::FullParallelism
                {
                    return Parallelism::WaitForFinished;
                }
                if self.parallelism(d.sub_jobs) != Parallelism::FullParallelism {
                    return Parallelism::WaitForFinished;
                }
                Parallelism::FullParallelism
            }
        }
    }

    /// `scheduleSelfOrChild()` of any job.
    fn schedule_self_or_child(&mut self, id: JobId) -> bool {
        match &self.nodes[id].kind {
            NodeKind::Item(_) => self.item_schedule_self_or_child(id),
            NodeKind::Composite(_) => self.composite_schedule_self_or_child(id),
            NodeKind::Bulk(_) => self.bulk_schedule_self_or_child(id),
            NodeKind::Directory(d) => {
                if d.root.is_some() {
                    self.root_schedule_self_or_child(id)
                } else {
                    self.directory_schedule_self_or_child(id)
                }
            }
        }
    }

    /// `PropagateItemJob::scheduleSelfOrChild`.
    fn item_schedule_self_or_child(&mut self, id: JobId) -> bool {
        if self.nodes[id].state != JobState::NotYetStarted {
            return false;
        }
        if let NodeKind::Item(j) = &self.nodes[id].kind {
            let i = j.item.borrow();
            log::info!(target: LOG, "Starting {:?} propagation of {} by {:?}", i.instruction, i.destination(), j.kind);
        }
        self.nodes[id].state = JobState::Running;
        self.item_start(id);
        true
    }

    /// `PropagatorCompositeJob::possiblyRunNextJob`.
    fn possibly_run_next_job(&mut self, composite: JobId, next: JobId) -> bool {
        if self.nodes[next].state == JobState::NotYetStarted {
            self.nodes[next].target = FinishTarget::Composite(composite);
        }
        self.schedule_self_or_child(next)
    }

    /// `PropagatorCompositeJob::scheduleSelfOrChild`.
    fn composite_schedule_self_or_child(&mut self, id: JobId) -> bool {
        if self.nodes[id].state == JobState::Finished {
            return false;
        }
        // Start the composite job
        if self.nodes[id].state == JobState::NotYetStarted {
            self.nodes[id].state = JobState::Running;
        }
        // Ask all the running composite jobs if they have something new to schedule.
        let running = self.composite(id).running_jobs.clone();
        for running_job in running {
            if !self.composite(id).running_jobs.contains(&running_job) {
                continue;
            }
            if self.possibly_run_next_job(id, running_job) {
                return true;
            }
            // If any of the running sub jobs is not parallel, we have to cancel the scheduling
            // of the rest of the list and wait for the blocking job to finish and schedule the next one.
            if self.parallelism(running_job) == Parallelism::WaitForFinished {
                return false;
            }
        }
        // Now it's our turn, check if we have something left to do.
        // First, convert a task to a job if necessary
        while self.composite(id).jobs_to_do.is_empty() {
            let Some(next_task) = self.composite_mut(id).tasks_to_do.pop_front() else {
                break;
            };
            let Some(job) = self.create_job(&next_task) else {
                if !self.is_delayed_upload_item(&next_task) {
                    let t = next_task.borrow();
                    log::warn!(target: "nextcloud.sync.propagator.directory", "Useless task found for file {} instruction {:?}", t.destination(), t.instruction);
                }
                continue;
            };
            self.composite_append_job(id, job);
            break;
        }
        // Then run the next job
        if let Some(next_job) = self.composite_mut(id).jobs_to_do.pop_front() {
            self.composite_mut(id).running_jobs.push(next_job);
            return self.possibly_run_next_job(id, next_job);
        }
        // If neither us or our children had stuff left to do we could hang. Make sure
        // we mark this job as finished so that the propagator can schedule a new one.
        let c = self.composite(id);
        if c.jobs_to_do.is_empty() && c.tasks_to_do.is_empty() && c.running_jobs.is_empty() {
            // Our parent jobs are already iterating over their running jobs, post to the event loop
            // to avoid removing ourself from that list while they iterate.
            self.posted.push_back(PostedEvent::CompositeFinalize(id));
        }
        false
    }

    /// `PropagatorCompositeJob::slotSubJobFinished`.
    fn composite_sub_job_finished(&mut self, id: JobId, sub_job: JobId, status: Status) {
        {
            let child_dir_item = match &self.nodes[sub_job].kind {
                NodeKind::Directory(d) => Some(d.item.clone()),
                NodeKind::Item(j) if j.kind == ItemKind::Ignore => Some(j.item.clone()),
                _ => None,
            };
            let c = self.composite(id);
            if (!c.is_any_invalid_char_child || !c.is_any_case_clash_child)
                && let Some(child) = child_dir_item
            {
                let ci = child.borrow();
                let clash = ci.status == Status::FileNameClash || ci.is_any_case_clash_child;
                let invalid = ci.status == Status::FileNameInvalid || ci.is_any_invalid_char_child;
                drop(ci);
                let c = self.composite_mut(id);
                c.is_any_case_clash_child = c.is_any_case_clash_child || clash;
                c.is_any_invalid_char_child = c.is_any_invalid_char_child || invalid;
            }
        }
        // Delete the job and remove it from our list of jobs.
        {
            let c = self.composite_mut(id);
            if let Some(i) = c.running_jobs.iter().position(|j| *j == sub_job) {
                c.running_jobs.remove(i);
            } else {
                debug_assert!(false, "slotSubJobFinished called twice");
            }
            // Any sub job error will cause the whole composite to fail.
            if matches!(
                status,
                Status::FatalError
                    | Status::NormalError
                    | Status::SoftError
                    | Status::DetailError
                    | Status::BlacklistedError
            ) {
                c.has_error = status;
            }
        }
        let c = self.composite(id);
        if c.jobs_to_do.is_empty() && c.tasks_to_do.is_empty() && c.running_jobs.is_empty() {
            self.composite_finalize(id);
        } else {
            self.shared.schedule_next_job();
        }
    }

    /// `PropagatorCompositeJob::finalize`.
    fn composite_finalize(&mut self, id: JobId) {
        // The propagator will do parallel scheduling and this could be posted
        // multiple times on the event loop, ignore the duplicate calls.
        if self.nodes[id].state == JobState::Finished {
            return;
        }
        self.nodes[id].state = JobState::Finished;
        let has_error = self.composite(id).has_error;
        let status = if has_error == Status::NoStatus {
            Status::Success
        } else {
            has_error
        };
        self.emit_job_finished(id, status);
    }

    /// `PropagateDirectory::scheduleSelfOrChild`.
    fn directory_schedule_self_or_child(&mut self, id: JobId) -> bool {
        if self.nodes[id].state == JobState::Finished {
            return false;
        }
        if self.nodes[id].state == JobState::NotYetStarted {
            self.nodes[id].state = JobState::Running;
        }
        let (first_job, sub_jobs) = {
            let d = self.directory(id);
            (d.first_job, d.sub_jobs)
        };
        if let Some(f) = first_job {
            match self.nodes[f].state {
                JobState::NotYetStarted => return self.schedule_self_or_child(f),
                // Don't schedule any more job until this is done.
                JobState::Running => return false,
                JobState::Finished => {}
            }
        }
        self.schedule_self_or_child(sub_jobs)
    }

    /// `PropagateRootDirectory::scheduleSelfOrChild`.
    fn root_schedule_self_or_child(&mut self, id: JobId) -> bool {
        if self.nodes[id].state == JobState::Finished {
            return false;
        }
        if self.directory_schedule_self_or_child(id) && self.delayed_tasks.is_empty() {
            return true;
        }
        // Important: Finish _subJobs before scheduling any deletes.
        let sub_jobs = self.directory(id).sub_jobs;
        if self.nodes[sub_jobs].state != JobState::Finished {
            return false;
        }
        if !self.delayed_tasks.is_empty() {
            log::debug!(target: "nextcloud.sync.propagator.root.directory", "root folder has more delayed jobs to do");
            return self.schedule_delayed_jobs(id);
        }
        let dir_deletion_jobs = self
            .directory(id)
            .root
            .as_ref()
            .expect("root")
            .dir_deletion_jobs;
        self.schedule_self_or_child(dir_deletion_jobs)
    }

    /// `PropagateDirectory::slotFirstJobFinished`.
    fn directory_first_job_finished(&mut self, id: JobId, status: Status) {
        self.directory_mut(id).first_job = None;
        if !matches!(
            status,
            Status::Success | Status::Restoration | Status::Conflict
        ) {
            if self.nodes[id].state != JobState::Finished {
                // Synchronously abort (the sub jobs never started)
                self.nodes[id].state = JobState::Finished;
                log::info!(target: LOG, "PropagateDirectory::slotFirstJobFinished emit finished {status:?}");
                self.emit_job_finished(id, status);
            }
            return;
        }
        self.shared.schedule_next_job();
    }

    /// `PropagateDirectory::slotSubJobsFinished`.
    fn directory_sub_jobs_finished(&mut self, id: JobId, mut status: Status) {
        if self.directory(id).root.is_some() {
            self.root_sub_jobs_finished(id, status);
            return;
        }
        let item = self.directory(id).item.clone();
        let sub_jobs = self.directory(id).sub_jobs;
        if !item.borrow().is_empty() && status == Status::Success {
            let (clash, invalid) = {
                let c = self.composite(sub_jobs);
                (c.is_any_case_clash_child, c.is_any_invalid_char_child)
            };
            {
                let mut i = item.borrow_mut();
                i.is_any_case_clash_child = i.is_any_case_clash_child || clash;
                i.is_any_invalid_char_child = i.is_any_invalid_char_child || invalid;
            }
            let (instruction, direction, original_file, rename_target, file, destination, modtime) = {
                let i = item.borrow();
                (
                    i.instruction,
                    i.direction,
                    i.original_file.clone(),
                    i.rename_target.clone(),
                    i.file.clone(),
                    i.destination().to_owned(),
                    i.modtime,
                )
            };
            // If a directory is renamed, recursively delete any stale items
            // that may still exist below the old path.
            if instruction == Instruction::Rename
                && original_file != rename_target
                && self
                    .shared
                    .journal
                    .delete_file_record(&original_file, true)
                    .is_err()
            {
                log::warn!(target: "nextcloud.sync.propagator.directory", "could not delete file from local DB {original_file}");
                self.nodes[id].state = JobState::Finished;
                status = Status::FatalError;
                {
                    let mut i = item.borrow_mut();
                    i.status = status;
                    i.error_string = format!("Could not delete file {original_file} from local DB");
                }
                self.emit_job_finished(id, status);
                return;
            }
            if instruction == Instruction::New && direction == Direction::Down {
                // special case for local MKDIR, set local directory mtime
                if modtime <= 0 {
                    status = Status::NormalError;
                    let mut i = item.borrow_mut();
                    i.status = status;
                    i.error_string =
                        "Error updating metadata due to invalid modification time".to_owned();
                }
                filesystem::set_mod_time(&self.shared.full_local_path(&destination), modtime);
            }
            // For new directories we always want to update the etag once
            // the directory has been propagated.
            if matches!(
                instruction,
                Instruction::Rename | Instruction::New | Instruction::UpdateMetadata
            ) {
                let perm = item.borrow().remote_perm;
                let read_only = !perm.is_null() && !perm.has_permissions_for_read_write();
                let wanted = if read_only {
                    filesystem::FolderPermissions::ReadOnly
                } else {
                    filesystem::FolderPermissions::ReadWrite
                };
                let file_name = self.shared.full_local_path(&file);
                if filesystem::file_exists(&file_name) {
                    filesystem::set_folder_permissions(&file_name, wanted);
                    self.shared.emit(PropagatorEvent::TouchedFile(file_name));
                }
                if !rename_target.is_empty() {
                    let file_name = self.shared.full_local_path(&rename_target);
                    if filesystem::file_exists(&file_name) {
                        filesystem::set_folder_permissions(&file_name, wanted);
                        self.shared.emit(PropagatorEvent::TouchedFile(file_name));
                    }
                }
                let (any_clash, any_invalid) = {
                    let i = item.borrow();
                    (i.is_any_case_clash_child, i.is_any_invalid_char_child)
                };
                if !any_clash && !any_invalid {
                    let result = self.shared.update_metadata(&item.borrow());
                    if let Err(e) = result {
                        status = Status::FatalError;
                        let mut i = item.borrow_mut();
                        i.status = status;
                        i.error_string = format!("Error updating metadata: {e}");
                        log::warn!(target: "nextcloud.sync.propagator.directory", "Error writing to the database for file {} with {e}", i.file);
                    }
                }
            }
        }
        self.nodes[id].state = JobState::Finished;
        log::debug!(target: "nextcloud.sync.propagator.directory", "PropagateDirectory::slotSubJobsFinished emit finished {status:?}");
        self.emit_job_finished(id, status);
    }

    /// `PropagateRootDirectory::slotSubJobsFinished`.
    fn root_sub_jobs_finished(&mut self, id: JobId, status: Status) {
        if !self.delayed_tasks.is_empty() {
            self.schedule_delayed_jobs(id);
            return;
        }
        if status == Status::FatalError {
            if self.nodes[id].state != JobState::Finished {
                // Synchronously abort
                self.nodes[id].state = JobState::Finished;
                log::info!(target: "nextcloud.sync.propagator.root.directory", "PropagateRootDirectory::slotSubJobsFinished emit finished {status:?}");
                self.emit_job_finished(id, status);
            }
            return;
        }
        let extras = self.directory_mut(id).root.as_mut().expect("root");
        if extras.error_status == Status::NoStatus {
            match status {
                Status::NoStatus | Status::FileIgnored | Status::Restoration | Status::Success => {}
                _ => extras.error_status = status,
            }
        }
        self.shared.schedule_next_job();
    }

    /// `PropagateRootDirectory::slotDirDeletionJobsFinished`.
    fn root_dir_deletion_jobs_finished(&mut self, id: JobId, mut status: Status) {
        let error_status = self.directory(id).root.as_ref().expect("root").error_status;
        if error_status != Status::NoStatus && status == Status::Success {
            status = error_status;
        }
        self.nodes[id].state = JobState::Finished;
        self.emit_job_finished(id, status);
    }

    /// Delivers a job's `finished(status)` signal.
    fn emit_job_finished(&mut self, id: JobId, status: Status) {
        match self.nodes[id].target {
            FinishTarget::None => {}
            FinishTarget::Composite(c) => self.composite_sub_job_finished(c, id, status),
            FinishTarget::DirectoryFirstJob(d) => self.directory_first_job_finished(d, status),
            FinishTarget::DirectorySubJobs(d) => self.directory_sub_jobs_finished(d, status),
            FinishTarget::RootDirDeletion(r) => self.root_dir_deletion_jobs_finished(r, status),
            FinishTarget::Propagator => self.emit_finished(status),
        }
    }

    // ------------------------------------------------------------------
    // Item jobs

    fn item_job(&self, id: JobId) -> &ItemJob {
        match &self.nodes[id].kind {
            NodeKind::Item(j) => j,
            _ => panic!("not an item job"),
        }
    }

    /// `start()` of an item job.
    fn item_start(&mut self, id: JobId) {
        let (item, kind, delete_existing, trash) = {
            let j = self.item_job(id);
            (
                j.item.clone(),
                j.kind.clone(),
                j.delete_existing,
                j.delete_to_client_trash_bin.clone(),
            )
        };
        let associated_composite = self.nodes[id].associated_composite;
        let ctx = JobCtx {
            shared: self.shared.clone(),
            id,
            item: item.clone(),
            associated_composite,
            delete_existing,
        };
        let sync_outcome: Option<Outcome> = match kind {
            ItemKind::Ignore => Some(Some(self.ignore_job_start(&item))),
            ItemKind::VfsUpdateMetadata => Some(Some(Done::ok())),
            ItemKind::LocalRemove => Some(local::local_remove(&ctx, &trash)),
            ItemKind::LocalMkdir => Some(local::local_mkdir(&ctx)),
            ItemKind::LocalRename => Some(local::local_rename(&ctx)),
            ItemKind::RemoteDelete => {
                self.spawn(id, Box::pin(remote::remote_delete(ctx)));
                None
            }
            ItemKind::RemoteMkdir => {
                self.spawn(id, Box::pin(remote::remote_mkdir(ctx)));
                None
            }
            ItemKind::RemoteMove => {
                self.spawn(id, Box::pin(remote::remote_move(ctx)));
                None
            }
            ItemKind::Download => {
                self.spawn(id, Box::pin(download::download(ctx)));
                None
            }
            ItemKind::UploadV1 => {
                self.spawn(id, Box::pin(upload::upload(ctx, false)));
                None
            }
            ItemKind::UploadNg => {
                self.spawn(id, Box::pin(upload::upload(ctx, true)));
                None
            }
        };
        if let Some(outcome) = sync_outcome {
            self.apply_tree_ops();
            if let Some(done) = outcome {
                self.item_done(id, done);
            }
        }
    }

    /// Starts an async item job: polled once right away, like upstream's
    /// synchronous `start()`.
    fn spawn(&mut self, id: JobId, mut fut: LocalBoxFuture<'static, Outcome>) {
        match poll_once(&mut fut) {
            Poll::Ready(outcome) => {
                self.apply_tree_ops();
                if let Some(done) = outcome {
                    self.item_done(id, done);
                }
            }
            Poll::Pending => {
                self.futures
                    .push(Box::pin(async move { (id, Completion::Item(fut.await)) }));
            }
        }
    }

    /// `PropagateIgnoreJob::start`.
    fn ignore_job_start(&self, item: &SyncFileItemPtr) -> Done {
        let mut i = item.borrow_mut();
        let mut status = i.status;
        if status == Status::NoStatus {
            if i.instruction == Instruction::Error {
                status = Status::NormalError;
            } else {
                status = Status::FileIgnored;
                debug_assert!(i.instruction == Instruction::Ignore);
            }
        } else if status == Status::FileNameClash {
            let conflict_record = self.shared.journal.case_conflict_record_by_path(&i.file);
            if conflict_record.is_valid() {
                i.file = String::from_utf8_lossy(&conflict_record.initial_base_path).into_owned();
            }
        }
        Done::new(status, i.error_string.clone(), ErrorCategory::NoError)
    }

    /// `PropagateItemJob::done`.
    fn item_done(&mut self, id: JobId, done: Done) {
        // Duplicate calls to done() are a logic error
        debug_assert!(self.nodes[id].state != JobState::Finished);
        self.nodes[id].state = JobState::Finished;
        let item = self.item_job(id).item.clone();
        let status = {
            let mut i = item.borrow_mut();
            i.status = done.status;
            crate::client_status_reporting::report_client_statuses(&self.shared.account, &i);
            if i.is_restoration {
                if i.status == Status::Success || i.status == Status::Conflict {
                    i.status = Status::Restoration;
                } else {
                    i.error_string = format!(
                        "{}. Restoration failed: {}",
                        i.error_string, done.error_string
                    );
                }
            } else if i.error_string.is_empty() {
                i.error_string = done.error_string.clone();
            }
            if self.shared.abort_requested.get()
                && matches!(i.status, Status::NormalError | Status::FatalError)
            {
                // an abort request is ongoing. Change the status to Soft-Error
                i.status = Status::SoftError;
            }
            // Blacklist handling
            match i.status {
                Status::SoftError
                | Status::FatalError
                | Status::NormalError
                | Status::DetailError => {
                    // Check the blacklist, possibly adjusting the item (including its status)
                    blacklist_update(&self.shared.journal, &mut i);
                }
                Status::Success | Status::Restoration if i.has_blacklist_entry => {
                    // wipe blacklist entry.
                    self.shared.journal.wipe_error_blacklist_entry(&i.file);
                    // remove a blacklist entry in case the file was moved.
                    if i.original_file != i.file {
                        self.shared
                            .journal
                            .wipe_error_blacklist_entry(&i.original_file);
                    }
                }
                _ => {}
            }
            i.status
        };
        self.emit_item_completed(&item, done.category);
        self.emit_job_finished(id, status);
        if status == Status::FatalError {
            // Abort all remaining jobs.
            self.abort();
        }
    }

    /// `emitItemCompleted`.
    fn emit_item_completed(&mut self, item: &SyncFileItemPtr, category: ErrorCategory) {
        {
            let i = item.borrow();
            if i.has_error_status() {
                log::warn!(target: LOG, "Could not complete propagation of {} with status {:?} and error: {}", i.destination(), i.status, i.error_string);
            } else {
                log::info!(target: LOG, "Completed propagation of {} with status {:?}", i.destination(), i.status);
            }
        }
        if let Some(cb) = self.callbacks.item_completed.as_mut() {
            cb(item, category);
        }
    }
}

/// The `PollJob` of `CleanupPollsJob`: polls an asynchronous upload of a
/// previous sync; the result is in the item's status, etag and file id.
pub async fn poll_for_cleanup(
    account: Arc<Account>,
    journal: Arc<SyncJournalDb>,
    local_path: &str,
    url: &str,
    item: &SyncFileItemPtr,
) {
    let p = Propagator::new(account, local_path, "/", journal, SyncOptions::default());
    let ctx = JobCtx {
        shared: p.shared.clone(),
        id: 0,
        item: item.clone(),
        associated_composite: None,
        delete_existing: false,
    };
    upload::poll_job(&ctx, url).await;
}

/// Aborts a running propagation from a callback (`OwncloudPropagator::abort`).
#[derive(Clone)]
pub struct AbortHandle {
    shared: std::rc::Weak<Shared>,
}

impl AbortHandle {
    pub fn abort(&self) {
        if let Some(s) = self.shared.upgrade() {
            s.request_abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_classify_error() {
        assert_eq!(
            classify_error(NetworkError::RemoteHostClosedError, 0, None, b""),
            Status::NormalError
        );
        assert_eq!(
            classify_error(NetworkError::OperationCanceledError, 0, None, b""),
            Status::FatalError
        );
        assert_eq!(
            classify_error(NetworkError::UnknownContentError, 412, None, b""),
            Status::SoftError
        );
        let another = Cell::new(false);
        assert_eq!(
            classify_error(NetworkError::UnknownContentError, 423, Some(&another), b""),
            Status::FileLocked
        );
        assert!(another.get());
        assert_eq!(
            classify_error(
                NetworkError::ServiceUnavailableError,
                503,
                None,
                br">Sabre\DAV\Exception\ServiceUnavailable<"
            ),
            Status::FatalError
        );
        assert_eq!(
            classify_error(NetworkError::InternalServerError, 500, None, b""),
            Status::NormalError
        );
    }
}
