// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/folder.{h,cpp}` (nextcloud/desktop v34.0.5),
// without the virtual files, the GUI (popups, activities, dialogs), the
// socket API and end-to-end encryption.

//! `Folder`: one synchronized folder, its sync engine, its journal, its
//! folder watcher and its sync state.
//!
//! The folder manager owns the folders and dispatches their events; the
//! methods that emit a signal upstream return [`FolderAction`]s instead
//! (`slotScheduleThisFolder` → [`FolderAction::Schedule`], ...).
//!
//! While a sync runs the engine is moved into the task that runs it and
//! comes back with [`FolderEvent::EngineFinished`]; `isSyncRunning()` is
//! "the engine is away". The pieces of the engine that upstream reads
//! while it syncs (the touched files of `wasFileTouched`, the exclude
//! engine, the abort) are shared handles.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use nc_dav::Account;
use nc_journal::exclude::ExcludedFiles;
use nc_journal::journal::{SelectiveSyncListType, SyncJournalDb, SyncJournalFileRecord};
use nc_sync::discovery::LocalDiscoveryStyle;
use nc_sync::touched_files::TouchedFiles;
use nc_sync::{
    AnotherSyncNeeded, EngineAbortHandle, ErrorCategory, Instruction, LocalDiscoveryTracker,
    LockOwnerType, LockStatus, ProgressInfo, Status as ItemStatus, SyncEngine, SyncFileItemPtr,
    SyncOptions,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::{Event, FolderEvent, FolderId, LockFileStateFinished, WatcherEvent};
use crate::network_limits::NetworkLimits;
use crate::sync_result::{SyncResult, SyncStatus};
use crate::timer::{Timer, single_shot};

const LOG: &str = "nextcloud.gui.folder";

/// The folder's settings (`FolderDefinition`, the fields this client uses).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Definition {
    /// The name of the folder in the ui and internally
    pub alias: String,
    /// path on local machine (always trailing /)
    pub local_path: String,
    /// path to the journal, usually relative to localPath
    pub journal_path: String,
    /// path on remote (usually no trailing /, exception "/")
    pub target_path: String,
    /// whether the folder is paused
    pub paused: bool,
    /// whether the folder syncs hidden files
    pub ignore_hidden_files: bool,
}

impl Definition {
    /// `absoluteJournalPath()`: journalPath relative to localPath.
    pub fn absolute_journal_path(&self) -> String {
        if self.journal_path.starts_with('/') {
            self.journal_path.clone()
        } else {
            format!(
                "{}{}",
                nc_journal::utility::trailing_slash_path(&self.local_path),
                self.journal_path
            )
        }
    }
}

/// The `ConfigFile` values a folder reads (`initializeSyncOptions`,
/// `startSync`, the big folder handlers).
#[derive(Clone, Debug)]
pub struct FolderSettings {
    /// `newBigFolderSizeLimit()`: (enabled, MB).
    pub new_big_folder_size_limit: (bool, i64),
    pub confirm_external_storage: bool,
    pub move_to_trash: bool,
    pub min_chunk_size: i64,
    pub max_chunk_size: i64,
    pub chunk_size: i64,
    /// `fullLocalDiscoveryInterval()`, with `OWNCLOUD_FULL_LOCAL_DISCOVERY_INTERVAL`
    /// applied; `None` = never force one (a negative value upstream).
    pub full_local_discovery_interval: Option<Duration>,
    pub stop_syncing_existing_folders_over_limit: bool,
    pub notify_existing_folders_over_limit: bool,
    /// `promptDeleteFiles()` and `deleteFilesThreshold()`.
    pub prompt_delete_files: bool,
    pub delete_files_threshold: i64,
    /// `ConfigFile::setupDefaultExcludeFilePaths`: the system and user
    /// exclude lists.
    pub exclude_files: Vec<String>,
    /// `SyncEngine::minimumFileAgeForUpload` (also `_scheduleSelfTimer`).
    pub minimum_file_age_for_upload: Duration,
}

impl Default for FolderSettings {
    fn default() -> Self {
        Self {
            new_big_folder_size_limit: (true, 500),
            confirm_external_storage: true,
            move_to_trash: false,
            min_chunk_size: 5 * 1000 * 1000,
            max_chunk_size: 5 * 1000 * 1000 * 1000,
            chunk_size: 100 * 1000 * 1000,
            full_local_discovery_interval: Some(Duration::from_secs(3600)),
            stop_syncing_existing_folders_over_limit: false,
            notify_existing_folders_over_limit: false,
            prompt_delete_files: false,
            delete_files_threshold: 100,
            exclude_files: Vec::new(),
            minimum_file_age_for_upload: Duration::from_millis(2000),
        }
    }
}

/// What a folder asks the folder manager to do (the signals and the
/// `FolderMan::instance()` calls of upstream).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderAction {
    /// `slotScheduleThisFolder()` → `FolderMan::scheduleFolder(this)`.
    Schedule,
    /// `FolderMan::scheduleFolderForImmediateSync(this)`.
    ScheduleImmediate,
    /// `FolderMan::setSyncEnabled(enabled)`.
    SetSyncEnabled(bool),
    /// `FolderMan::slotScheduleETagJob(alias, job)`.
    EtagJobCreated,
    /// `accountState()->tagLastSuccessfullETagRequest(time)`.
    TagLastSuccessfulEtagRequest(SystemTime),
    /// `syncStarted()`.
    SyncStarted,
    /// `syncFinished(result)`.
    SyncFinished,
    /// `syncStateChange()`.
    SyncStateChange,
    /// `syncPausedChanged(this, paused)`.
    SyncPausedChanged(bool),
    /// `canSyncChanged()`.
    CanSyncChanged,
    /// `saveToSettings()`: the definition changed.
    SaveToSettings,
}

/// The upstream `_requestEtagJob`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EtagJob {
    None,
    /// Created and handed to the folder manager's queue.
    Queued,
    /// Started by the folder manager.
    Running,
}

/// The folder watcher seen from the folder (`FolderWatcher` methods it
/// calls).
pub trait FolderWatcherHandle {
    /// `isReliable()`.
    fn is_reliable(&self) -> bool;
    /// `canSetPermissions()`.
    fn can_set_permissions(&self) -> bool;
    /// `slotLockFileDetectedExternally(lockFile)`.
    fn slot_lock_file_detected_externally(&self, _lock_file: &str) {}
}

/// Creates the folder watcher of a folder (`registerFolderWatcher`):
/// `(folder id, canonical path with a trailing '/', files locking
/// available, event queue)`.
pub type WatcherFactory = Rc<
    dyn Fn(FolderId, &str, bool, &UnboundedSender<Event>) -> Option<Box<dyn FolderWatcherHandle>>,
>;

/// `Folder`.
pub struct Folder {
    id: FolderId,
    account_id: String,
    account: Arc<Account>,
    definition: Definition,
    /// As returned with QFileInfo:canonicalFilePath. Always ends with "/"
    canonical_local_path: String,
    sync_result: SyncResult,
    engine: Option<SyncEngine>,
    engine_abort: Option<EngineAbortHandle>,
    /// The engine's limits, changeable while it syncs.
    engine_limits: Option<Arc<nc_sync::propagator::NetworkLimits>>,
    touched_files: Rc<RefCell<TouchedFiles>>,
    excluded_files: Rc<RefCell<ExcludedFiles>>,
    ignore_hidden: Rc<Cell<bool>>,
    journal: Arc<SyncJournalDb>,
    etag_job: EtagJob,
    last_etag: Vec<u8>,
    time_since_last_sync_done: Instant,
    time_since_last_sync_start: Instant,
    time_since_last_full_local_discovery: Option<Instant>,
    last_sync_duration: Duration,
    /// The number of syncs that failed in a row.
    /// Reset when a sync is successful.
    consecutive_failing_syncs: u32,
    /// The number of requested follow-up syncs.
    /// Reset when no follow-up is requested.
    consecutive_follow_up_syncs: u32,
    schedule_self_timer: Timer,
    /// The engine's scheduled sync run (lock expiry) timer.
    scheduled_sync_timer: Timer,
    save_backwards_compatible: bool,
    silence_errors_until_next_sync: bool,
    folder_watcher: Option<Box<dyn FolderWatcherHandle>>,
    local_discovery_tracker: LocalDiscoveryTracker,
    /// The remote file ID of the current folder.
    root_file_id: i64,
    /// The last progress of the running sync (`ProgressDispatcher`).
    progress: Option<ProgressInfo>,
    settings: FolderSettings,
    network_limits: NetworkLimits,
    tx: UnboundedSender<Event>,
}

impl Folder {
    /// `Folder(definition, accountState, vfs)`.
    pub fn new(
        id: FolderId,
        definition: Definition,
        account_id: &str,
        account: Arc<Account>,
        settings: FolderSettings,
        network_limits: NetworkLimits,
        tx: &UnboundedSender<Event>,
    ) -> Self {
        let mut definition = definition;
        definition.local_path = nc_journal::utility::trailing_slash_path(&definition.local_path);
        definition.target_path = prepare_target_path(&definition.target_path);
        let journal = Arc::new(SyncJournalDb::new(definition.absolute_journal_path()));
        let mut sync_result = SyncResult::new();
        let status = if definition.paused {
            SyncStatus::Paused
        } else {
            SyncStatus::NotYetStarted
        };
        sync_result.set_status(status);
        let mut folder = Self {
            id,
            account_id: account_id.to_owned(),
            account: account.clone(),
            canonical_local_path: String::new(),
            sync_result,
            engine: None,
            engine_abort: None,
            engine_limits: None,
            touched_files: Rc::new(RefCell::new(TouchedFiles::new())),
            excluded_files: Rc::new(RefCell::new(ExcludedFiles::new(&definition.local_path))),
            ignore_hidden: Rc::new(Cell::new(definition.ignore_hidden_files)),
            journal: journal.clone(),
            etag_job: EtagJob::None,
            last_etag: Vec::new(),
            time_since_last_sync_done: Instant::now(),
            time_since_last_sync_start: Instant::now(),
            time_since_last_full_local_discovery: None,
            last_sync_duration: Duration::ZERO,
            consecutive_failing_syncs: 0,
            consecutive_follow_up_syncs: 0,
            schedule_self_timer: Timer::new(settings.minimum_file_age_for_upload, true),
            scheduled_sync_timer: Timer::new(Duration::ZERO, true),
            save_backwards_compatible: false,
            silence_errors_until_next_sync: false,
            folder_watcher: None,
            local_discovery_tracker: LocalDiscoveryTracker::new(),
            root_file_id: 0,
            progress: None,
            definition,
            settings,
            network_limits,
            tx: tx.clone(),
        };
        // check if the local path exists
        folder.check_local_path();
        folder.sync_result.set_folder(&folder.definition.alias);
        let options = folder.initialize_sync_options();
        let path = folder.path().to_owned();
        let mut engine = SyncEngine::new(
            account,
            &path,
            options,
            &folder.definition.target_path,
            journal,
        );
        // pass the setting if hidden files are to be ignored, will be read in csync_update
        engine.set_ignore_hidden_files(folder.definition.ignore_hidden_files);
        engine.prompt_delete_files = folder.settings.prompt_delete_files;
        engine.delete_files_threshold = folder.settings.delete_files_threshold;
        for file in &folder.settings.exclude_files {
            engine.excluded_files().add_exclude_file_path(file);
        }
        if !engine.excluded_files().reload_exclude_files() {
            log::warn!(target: LOG, "Could not read system exclude file");
        }
        folder.engine_limits = Some(engine.network_limits());
        folder.touched_files = engine.touched_files();
        folder.excluded_files = engine.excluded_files_handle();
        folder.connect_engine(&mut engine);
        folder.engine = Some(engine);
        folder
    }

    /// The engine's signals, posted to the queue (`connect(_engine, ...)`).
    fn connect_engine(&self, engine: &mut SyncEngine) {
        let id = self.id;
        let mut cbs = engine.callbacks();
        let post = |tx: &UnboundedSender<Event>| {
            let tx = tx.clone();
            move |e: FolderEvent| {
                let _ = tx.send(Event::Folder(id, e));
            }
        };
        let p = post(&self.tx);
        cbs.started = Some(Box::new(move || p(FolderEvent::EngineStarted)));
        let p = post(&self.tx);
        cbs.item_completed = Some(Box::new(move |item, cat| {
            p(FolderEvent::ItemCompleted(item.clone(), cat))
        }));
        let p = post(&self.tx);
        cbs.transmission_progress = Some(Box::new(move |pi| {
            p(FolderEvent::TransmissionProgress(Box::new(pi.clone())))
        }));
        let p = post(&self.tx);
        cbs.sync_error = Some(Box::new(move |msg, cat| {
            p(FolderEvent::SyncError(msg.to_owned(), cat))
        }));
        let p = post(&self.tx);
        cbs.root_etag = Some(Box::new(move |etag, time| {
            p(FolderEvent::RootEtag(etag.to_vec(), time))
        }));
        let p = post(&self.tx);
        cbs.lock_file_detected = Some(Box::new(move |lock_file| {
            p(FolderEvent::LockFileDetected(lock_file.to_owned()))
        }));
        let p = post(&self.tx);
        cbs.root_file_id_received =
            Some(Box::new(move |file_id| p(FolderEvent::RootFileId(file_id))));
        // slotNewBigFolderDiscovered and slotExistingFolderNowBig change the
        // selective sync lists of the journal right away (direct connections
        // upstream): the discovery reads them back in the same run.
        let journal = self.journal.clone();
        let p = post(&self.tx);
        let limit_mb = self.settings.new_big_folder_size_limit.1;
        cbs.new_big_folder = Some(Box::new(move |new_f, is_external| {
            slot_new_big_folder_discovered(&journal, new_f, is_external, limit_mb);
            p(FolderEvent::NewBigFolder(new_f.to_owned(), is_external));
        }));
        let p = post(&self.tx);
        cbs.existing_folder_now_big = Some(Box::new(move |f| {
            p(FolderEvent::ExistingFolderNowBig(f.to_owned()))
        }));
        // slotAboutToRemoveAllFiles: upstream asks the user with a dialog
        // ("Proceed with Deletion" / "Restore Files"). There is nobody to
        // ask: the daemon answers "Restore", which loses no data.
        let p = post(&self.tx);
        let alias = self.definition.alias.clone();
        cbs.about_to_remove_all_files = Some(Box::new(move |dir| {
            log::warn!(target: LOG, "Folder {alias}: a large number of files would be deleted ({dir:?}); not deleting them, restoring them instead (no one to confirm the deletion)");
            p(FolderEvent::AllFilesDeletedRestore);
            true
        }));
    }

    pub fn id(&self) -> FolderId {
        self.id
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn account(&self) -> &Arc<Account> {
        &self.account
    }

    pub fn definition(&self) -> &Definition {
        &self.definition
    }

    /// `alias()`.
    pub fn alias(&self) -> &str {
        &self.definition.alias
    }

    /// `path()`: the canonical local path, with a trailing '/'.
    pub fn path(&self) -> &str {
        &self.canonical_local_path
    }

    /// `cleanPath()`.
    pub fn clean_path(&self) -> String {
        nc_sync::touched_files::clean_path(&self.canonical_local_path)
    }

    /// `remotePath()`.
    pub fn remote_path(&self) -> &str {
        &self.definition.target_path
    }

    /// `remotePathTrailingSlash()`.
    pub fn remote_path_trailing_slash(&self) -> String {
        nc_journal::utility::trailing_slash_path(&self.definition.target_path)
    }

    /// `remoteUrl()`.
    pub fn remote_url(&self) -> String {
        nc_dav::account::concat_url_path(&self.account.dav_url_string(), self.remote_path())
    }

    pub fn journal(&self) -> &Arc<SyncJournalDb> {
        &self.journal
    }

    /// The engine (`None` while it syncs).
    pub fn sync_engine(&mut self) -> Option<&mut SyncEngine> {
        self.engine.as_mut()
    }

    pub fn progress(&self) -> Option<&ProgressInfo> {
        self.progress.as_ref()
    }

    /// `checkLocalPath()`.
    fn check_local_path(&mut self) -> bool {
        let local = &self.definition.local_path;
        let canonical = std::fs::canonicalize(local)
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        self.canonical_local_path = match canonical {
            Some(c) if !c.is_empty() => c,
            _ => {
                log::warn!(target: LOG, "Broken symlink: {local}");
                local.clone()
            }
        };
        self.canonical_local_path =
            nc_journal::utility::trailing_slash_path(&self.canonical_local_path);
        let meta = std::fs::metadata(local);
        let readable = std::fs::read_dir(local).is_ok();
        if meta.as_ref().is_ok_and(|m| m.is_dir()) && readable {
            log::debug!(target: LOG, "Checked local path ok");
            return true;
        }
        let error = match meta {
            Err(_) => {
                format!("Please choose a different location. The folder {local} doesn't exist.")
            }
            Ok(m) if !m.is_dir() => {
                format!("Please choose a different location. {local} isn't a valid folder.")
            }
            Ok(_) => {
                format!("Please choose a different location. {local} isn't a readable folder.")
            }
        };
        self.sync_result.append_error_string(&error);
        self.sync_result.set_status(SyncStatus::SetupError);
        false
    }

    /// `ignoreHiddenFiles()`.
    pub fn ignore_hidden_files(&self) -> bool {
        self.definition.ignore_hidden_files
    }

    /// `setIgnoreHiddenFiles(ignore)`.
    pub fn set_ignore_hidden_files(&mut self, ignore: bool) {
        self.definition.ignore_hidden_files = ignore;
        self.ignore_hidden.set(ignore);
    }

    /// `isBusy()`.
    pub fn is_busy(&self) -> bool {
        self.is_sync_running()
    }

    /// `isSyncRunning()`.
    pub fn is_sync_running(&self) -> bool {
        self.engine.is_none()
    }

    /// `syncPaused()`.
    pub fn sync_paused(&self) -> bool {
        self.definition.paused
    }

    /// `canSync()`, given `accountState()->isConnected()`.
    pub fn can_sync(&self, account_connected: bool) -> bool {
        !self.sync_paused()
            && account_connected
            && self.sync_result.status() != SyncStatus::SetupError
    }

    /// `setSyncPaused(paused)`.
    pub fn set_sync_paused(&mut self, paused: bool) -> Vec<FolderAction> {
        if paused == self.definition.paused {
            return Vec::new();
        }
        self.definition.paused = paused;
        if !paused {
            self.set_sync_state(SyncStatus::NotYetStarted);
        } else {
            self.set_sync_state(SyncStatus::Paused);
        }
        vec![
            FolderAction::SaveToSettings,
            FolderAction::SyncPausedChanged(paused),
            FolderAction::SyncStateChange,
            FolderAction::CanSyncChanged,
        ]
    }

    /// `setSyncState(state)`.
    pub fn set_sync_state(&mut self, state: SyncStatus) {
        if self.silence_errors_until_next_sync && state == SyncStatus::Error {
            self.sync_result.set_status(SyncStatus::Success);
            return;
        }
        self.sync_result.set_status(state);
    }

    /// `syncResult()`.
    pub fn sync_result(&self) -> &SyncResult {
        &self.sync_result
    }

    /// `prepareToSync()`.
    pub fn prepare_to_sync(&mut self) {
        self.sync_result.reset();
        self.sync_result.set_status(SyncStatus::NotYetStarted);
    }

    pub fn etag_job(&self) -> EtagJob {
        self.etag_job
    }

    /// `msecSinceLastSync()`.
    pub fn msec_since_last_sync(&self) -> Duration {
        self.time_since_last_sync_done.elapsed()
    }

    /// `msecLastSyncDuration()`.
    pub fn msec_last_sync_duration(&self) -> Duration {
        self.last_sync_duration
    }

    pub fn consecutive_follow_up_syncs(&self) -> u32 {
        self.consecutive_follow_up_syncs
    }

    pub fn consecutive_failing_syncs(&self) -> u32 {
        self.consecutive_failing_syncs
    }

    /// `syncEngine().isAnotherSyncNeeded()` (no follow-up while it runs).
    pub fn another_sync_needed(&self) -> AnotherSyncNeeded {
        self.engine
            .as_ref()
            .map_or(AnotherSyncNeeded::NoFollowUpSync, |e| {
                e.is_another_sync_needed()
            })
    }

    pub fn set_save_backwards_compatible(&mut self, save: bool) {
        self.save_backwards_compatible = save;
    }

    pub fn save_backwards_compatible(&self) -> bool {
        self.save_backwards_compatible
    }

    /// `setDirtyNetworkLimits()` with new account limits.
    pub fn set_network_limits(&mut self, limits: NetworkLimits) {
        self.network_limits = limits;
        self.set_dirty_network_limits();
    }

    /// `setDirtyNetworkLimits()`: a running sync picks the new limits up
    /// within 10 s (the bandwidth manager's switching timer).
    pub fn set_dirty_network_limits(&mut self) {
        let (up, down) = self.network_limits.engine_limits();
        if let Some(l) = &self.engine_limits {
            l.set(up, down);
        }
    }

    /// `slotRunEtagJob()`.
    pub fn slot_run_etag_job(&mut self, account_connected: bool) -> Vec<FolderAction> {
        log::info!(target: LOG, "Trying to check {} for changes via ETag check. (time since last sync: {} s)", self.remote_url(), self.time_since_last_sync_done.elapsed().as_secs());
        if self.etag_job != EtagJob::None {
            log::info!(target: LOG, "{} has ETag job queued, not trying to sync", self.remote_url());
            return Vec::new();
        }
        if !self.can_sync(account_connected) {
            log::info!(target: LOG, "Not syncing.  : {} {}", self.remote_url(), self.definition.paused);
            return Vec::new();
        }
        // Do the ordinary etag check for the root folder and schedule a
        // sync if it's different.
        self.etag_job = EtagJob::Queued;
        vec![FolderAction::EtagJobCreated]
    }

    /// Starts the queued etag job (`_currentEtagJob->start()`).
    pub fn start_etag_job(&mut self) {
        self.etag_job = EtagJob::Running;
        let account = self.account.clone();
        let path = self.remote_path().to_owned();
        let tx = self.tx.clone();
        let id = self.id;
        tokio::task::spawn_local(async move {
            let opts = nc_dav::JobOptions {
                timeout: Duration::from_secs(60),
                ..nc_dav::JobOptions::default()
            };
            let result = nc_dav::jobs::request_etag(&account, &path, &opts).await;
            let _ = tx.send(Event::Folder(
                id,
                FolderEvent::EtagJobFinished(result, SystemTime::now()),
            ));
        });
    }

    /// The etag job finished (it deletes itself upstream).
    pub fn etag_job_finished(
        &mut self,
        result: Result<Vec<u8>, nc_dav::HttpError>,
        time: SystemTime,
    ) -> Vec<FolderAction> {
        self.etag_job = EtagJob::None;
        match result {
            Ok(etag) => self.etag_retrieved(&etag, time),
            Err(e) => {
                log::warn!(target: LOG, "ETag job of {} failed: {} {}", self.remote_url(), e.code, e.message);
                Vec::new()
            }
        }
    }

    /// `etagRetrieved(etag, tp)`.
    fn etag_retrieved(&mut self, etag: &[u8], tp: SystemTime) -> Vec<FolderAction> {
        // re-enable sync if it was disabled because network was down
        let mut actions = vec![FolderAction::SetSyncEnabled(true)];
        if self.last_etag != etag {
            log::info!(target: LOG, "Compare etag with previous etag: last: {}, received: {} -> CHANGED", String::from_utf8_lossy(&self.last_etag), String::from_utf8_lossy(etag));
            self.last_etag = etag.to_vec();
            actions.push(FolderAction::Schedule);
        }
        actions.push(FolderAction::TagLastSuccessfulEtagRequest(tp));
        actions
    }

    /// `etagRetrievedFromSyncEngine(etag, time)`.
    pub fn etag_retrieved_from_sync_engine(
        &mut self,
        etag: &[u8],
        time: SystemTime,
    ) -> Vec<FolderAction> {
        log::info!(target: LOG, "Root etag from during sync: {}", String::from_utf8_lossy(etag));
        self.last_etag = etag.to_vec();
        vec![FolderAction::TagLastSuccessfulEtagRequest(time)]
    }

    /// `rootFileIdReceivedFromSyncEngine(fileId)`.
    pub fn root_file_id_received_from_sync_engine(&mut self, file_id: i64) {
        log::debug!(target: LOG, "retrieved root fileId={file_id}");
        self.root_file_id = file_id;
    }

    /// `slotDiscardDownloadProgress()`: deletes the partial downloads.
    pub fn slot_discard_download_progress(&self) -> usize {
        let keep_nothing = std::collections::HashSet::new();
        let deleted_infos = self
            .journal
            .get_and_delete_stale_download_infos(&keep_nothing);
        for deleted_info in &deleted_infos {
            let tmppath = format!(
                "{}{}",
                nc_journal::utility::trailing_slash_path(&self.definition.local_path),
                deleted_info.tmpfile
            );
            log::info!(target: LOG, "Deleting temporary file:  {tmppath}");
            let _ = std::fs::remove_file(&tmppath);
        }
        deleted_infos.len()
    }

    /// `downloadInfoCount()`.
    pub fn download_info_count(&self) -> i64 {
        self.journal.download_info_count()
    }

    /// `errorBlackListEntryCount()`.
    pub fn error_black_list_entry_count(&self) -> i64 {
        self.journal.error_black_list_entry_count()
    }

    /// `slotWipeErrorBlacklist()`.
    pub fn slot_wipe_error_blacklist(&self) -> i64 {
        self.journal.wipe_error_blacklist()
    }

    /// `pathIsIgnored(path)` (upstream skips it in its test builds).
    pub fn path_is_ignored(&self, path: &str) -> bool {
        path_is_ignored(
            &self.excluded_files,
            self.path(),
            self.ignore_hidden.get(),
            path,
        )
    }

    /// `isFileExcludedAbsolute(fullPath)`.
    pub fn is_file_excluded_absolute(&self, full_path: &str) -> bool {
        self.excluded_files
            .borrow()
            .is_excluded(full_path, self.path(), self.ignore_hidden.get())
    }

    /// `isFileExcludedRelative(relativePath)`.
    pub fn is_file_excluded_relative(&self, relative_path: &str) -> bool {
        self.is_file_excluded_absolute(&format!("{}{relative_path}", self.path()))
    }

    /// `slotWatchedPathChanged(path, reason)`; `unlock`:
    /// `reason == ChangeReason::UnLock`.
    pub fn slot_watched_path_changed(&mut self, path: &str, unlock: bool) -> Vec<FolderAction> {
        let Some(relative_path) = path.strip_prefix(self.path()) else {
            log::debug!(target: LOG, "Changed path is not contained in folder, ignoring: {path}");
            return Vec::new();
        };
        let relative_path = relative_path.to_owned();
        // With virtual files off (VfsOff), an ignored path only gets its pin
        // state set to "excluded", which VfsOff ignores.
        if self.path_is_ignored(path) {
            return Vec::new();
        }
        // Add to list of locally modified paths
        //
        // We do this before checking for our own sync-related changes to make
        // extra sure to not miss relevant changes.
        self.local_discovery_tracker
            .add_touched_path(&relative_path);
        // The folder watcher fires a lot of bogus notifications during
        // a sync operation, both for actual user files and the database
        // and log. Therefore we check notifications against operations
        // the sync is doing to filter out our own changes.
        if self.touched_files.borrow_mut().was_file_touched(path) {
            log::debug!(target: LOG, "Changed path was touched by SyncEngine, ignoring: {path}");
            return Vec::new();
        }
        log::debug!(target: LOG, "Detected changes in paths: {path}");
        let record = match self.journal.get_file_record(relative_path.as_bytes()) {
            Ok(r) => r,
            Err(_) => {
                log::warn!(target: LOG, "could not get file from local DB {relative_path}");
                None
            }
        };
        let record = record.filter(|r| r.is_valid());
        if !unlock {
            // Check that the mtime/size actually changed or there was
            // an attribute change (pin state) that caused the notification.
            // With VfsOff the pin state is always AlwaysLocal and every file
            // is "in sync" as a placeholder: an unchanged file is spurious
            // unless the record is a virtual file.
            if let Some(r) = &record
                && !nc_sync::filesystem::file_changed(path, r.file_size, r.modtime)
                && !r.is_virtual_file()
            {
                log::debug!(target: LOG, "Ignoring spurious notification for file {relative_path}");
                return Vec::new(); // probably a spurious notification
            }
        }
        self.warn_on_new_excluded_item(record.is_some(), &relative_path);
        // Also schedule this folder for a sync, but only after some delay:
        // The sync will not upload files that were changed too recently.
        self.schedule_this_folder_soon();
        Vec::new()
    }

    /// `warnOnNewExcludedItem(record, path)`.
    fn warn_on_new_excluded_item(&self, record_valid: bool, path: &str) {
        // Never warn for items in the database
        if record_valid {
            return;
        }
        // Don't warn for items that no longer exist.
        // Note: This assumes we're getting file watcher notifications
        // for folders only on creation and deletion - if we got a notification
        // on content change that would create spurious warnings.
        let full_path = format!("{}{path}", self.canonical_local_path);
        let Ok(meta) = std::fs::symlink_metadata(&full_path) else {
            return;
        };
        let Ok(selective_sync_list) = self
            .journal
            .get_selective_sync_list(SelectiveSyncListType::BlackList)
        else {
            return;
        };
        if !selective_sync_list.contains(&format!("{path}/")) {
            return;
        }
        if meta.is_dir() {
            log::warn!(target: LOG, "The folder {full_path} was created but was excluded from synchronization previously. Data inside it will not be synchronized.");
        } else {
            log::warn!(target: LOG, "The file {full_path} was created but was excluded from synchronization previously. It will not be synchronized.");
        }
    }

    /// `slotWatcherUnreliable(message)`.
    pub fn slot_watcher_unreliable(&self, message: &str) {
        log::warn!(target: LOG, "Folder watcher for {} became unreliable: {message}", self.path());
        log::warn!(target: LOG, "Changes in synchronized folders could not be tracked reliably. This means that the synchronization client might not upload local changes immediately and will instead only scan for local changes and upload them occasionally (every two hours by default). {message}");
    }

    /// The signals of the folder watcher (`registerFolderWatcher`).
    pub fn handle_watcher_event(&mut self, event: WatcherEvent) -> Vec<FolderAction> {
        match event {
            WatcherEvent::PathChanged(p) => self.slot_watched_path_changed(&p, false),
            WatcherEvent::LostChanges => {
                self.slot_next_sync_full_local_discovery();
                // Divergence (README, "Divergences from upstream"): only our
                // Linux watcher reports lost changes (IN_Q_OVERFLOW, which
                // upstream ignores), and the full local discovery must not
                // wait for an unrelated trigger, so a sync is scheduled.
                self.schedule_this_folder_soon();
                Vec::new()
            }
            WatcherEvent::BecameUnreliable(m) => {
                self.slot_watcher_unreliable(&m);
                Vec::new()
            }
            // `registerFolderWatcher` / `slotCapabilitiesChanged` connect
            // these two only when the account has the files locking
            // capability.
            WatcherEvent::FilesLockReleased(files) => {
                if self.account.capabilities().files_lock_available() {
                    self.slot_files_lock_released(&files);
                }
                Vec::new()
            }
            WatcherEvent::LockedFilesFound(files) => {
                if self.account.capabilities().files_lock_available() {
                    self.slot_locked_files_found(&files);
                }
                Vec::new()
            }
            WatcherEvent::FilesLockImposed(files) => {
                self.slot_files_lock_imposed(&files);
                Vec::new()
            }
        }
    }

    /// `fileFromLocalPath(localPath)`: `localPath.mid(cleanPath().length() + 1)`.
    fn file_from_local_path(&self, local_path: &str) -> String {
        let skip = self.clean_path().chars().count() + 1;
        local_path.chars().skip(skip).collect()
    }

    /// The valid journal record of `file_record_path`.
    fn valid_file_record(&self, file_record_path: &str) -> Option<SyncJournalFileRecord> {
        self.journal
            .get_file_record(file_record_path.as_bytes())
            .ok()
            .flatten()
            .filter(SyncJournalFileRecord::is_valid)
    }

    /// `slotFilesLockReleased(files)`: the lock files of these documents
    /// are gone; unlock on the server the ones this user locked.
    pub fn slot_files_lock_released(&mut self, files: &[String]) {
        log::debug!(target: LOG, "Going to unlock office files {files:?}");
        let files_lock_type_available = self.account.capabilities().files_lock_type_available();
        let dav_user = self.account.dav_user();
        for file in files {
            let file_record_path = self.file_from_local_path(file);
            let rec = self.valid_file_record(&file_record_path);
            // (VfsOff: `updatePlaceholderMarkInSync` does nothing.)
            let can_unlock_file = rec.as_ref().is_some_and(|rec| {
                rec.lockstate.locked
                    && (!files_lock_type_available
                        || rec.lockstate.lock_owner_type == LockOwnerType::TokenLock as i64)
                    && rec.lockstate.lock_owner_id == dav_user
            });
            let Some(rec) = rec.filter(|_| can_unlock_file) else {
                log::info!(target: LOG, "Skipping file {file} (not locked by {dav_user} with a lock this client can release)");
                continue;
            };
            let remote_file_path =
                format!("{}{}", self.remote_path_trailing_slash(), rec.path_str());
            log::debug!(target: LOG, "Unlocking an office file {remote_file_path}");
            let lock_owner_type = LockOwnerType::from_i64(rec.lockstate.lock_owner_type);
            self.set_lock_file_state(
                remote_file_path,
                &String::from_utf8_lossy(&rec.etag),
                LockStatus::UnlockedItem,
                lock_owner_type,
            );
        }
    }

    /// `slotFilesLockImposed(files)`: with virtual files off it only
    /// updates placeholders, which does nothing.
    pub fn slot_files_lock_imposed(&self, files: &[String]) {
        log::debug!(target: LOG, "Lock files detected for office files {files:?}");
    }

    /// `slotLockedFilesFound(files)`: an application opened these
    /// documents (lock files appeared); lock them on the server with a
    /// token lock.
    pub fn slot_locked_files_found(&mut self, files: &[String]) {
        log::debug!(target: LOG, "Found new lock files {files:?}");
        for file in files {
            let file_record_path = self.file_from_local_path(file);
            let Some(rec) = self
                .valid_file_record(&file_record_path)
                .filter(|rec| !rec.lockstate.locked)
            else {
                log::debug!(target: LOG, "Skipping locking file {file} (not in the journal or already locked)");
                continue;
            };
            let remote_file_path =
                format!("{}{}", self.remote_path_trailing_slash(), rec.path_str());
            log::debug!(target: LOG, "Automatically locking file on server {remote_file_path}");
            self.set_lock_file_state(
                remote_file_path,
                &String::from_utf8_lossy(&rec.etag),
                LockStatus::LockedItem,
                LockOwnerType::TokenLock,
            );
        }
    }

    /// `_accountState->account()->setLockFileState(remoteFilePath,
    /// remotePathTrailingSlash(), path(), etag, journalDb(), status, type)`;
    /// the result comes back as [`FolderEvent::LockFileStateFinished`].
    ///
    /// Upstream connects the account-wide `lockFileSuccess` /
    /// `lockFileError` signals for each request (one connection kept per
    /// folder, so a reply can be taken for another request's); here each
    /// request gets its own result.
    fn set_lock_file_state(
        &self,
        remote_file_path: String,
        etag: &str,
        lock_status: LockStatus,
        lock_owner_type: LockOwnerType,
    ) {
        let Some(request) = nc_sync::account_lock::set_lock_file_state(
            &self.account,
            &remote_file_path,
            &self.remote_path_trailing_slash(),
            self.path(),
            etag,
            &self.journal,
            lock_status,
            lock_owner_type,
        ) else {
            return;
        };
        let tx = self.tx.clone();
        let id = self.id;
        let lock = lock_status == LockStatus::LockedItem;
        tokio::task::spawn_local(async move {
            let result = request.await;
            let _ = tx.send(Event::Folder(
                id,
                FolderEvent::LockFileStateFinished(LockFileStateFinished {
                    remote_file_path,
                    lock,
                    result,
                }),
            ));
        });
    }

    /// The lambdas connected to `lockFileSuccess` / `lockFileError` by
    /// `slotFilesLockReleased` and `slotLockedFilesFound`. Returns true
    /// when the folder must `startSync()`.
    pub fn lock_file_state_finished(&mut self, finished: &LockFileStateFinished) -> bool {
        let path = &finished.remote_file_path;
        match (&finished.result, finished.lock) {
            (Ok(()), true) => {
                log::debug!(target: LOG, "Locking file succeeded {path}");
                true
            }
            (Ok(()), false) => {
                log::debug!(target: LOG, "Unlocking an office file succeeded {path}");
                true
            }
            (Err(message), true) => {
                log::warn!(target: LOG, "Failed to lock a file: {path} {message}");
                false
            }
            (Err(message), false) => {
                log::warn!(target: LOG, "Failed to unlock a file: {path} {message}");
                false
            }
        }
    }

    /// `SyncEngine::lockFileDetected` → `FolderWatcher::slotLockFileDetectedExternally`.
    pub fn slot_lock_file_detected(&self, lock_file: &str) {
        if let Some(w) = self.folder_watcher.as_ref() {
            w.slot_lock_file_detected_externally(lock_file);
        }
    }

    /// `registerFolderWatcher()`.
    pub fn register_folder_watcher(&mut self, factory: &WatcherFactory) {
        if self.folder_watcher.is_some() {
            return;
        }
        if !std::path::Path::new(self.path()).is_dir() {
            return;
        }
        let files_lock = self.account.capabilities().files_lock_available();
        self.folder_watcher = factory(self.id, self.path(), files_lock, &self.tx);
    }

    /// `disconnectFolderWatcher()`.
    pub fn disconnect_folder_watcher(&mut self) {
        self.folder_watcher = None;
    }

    pub fn has_folder_watcher(&self) -> bool {
        self.folder_watcher.is_some()
    }

    /// `slotTerminateSync()`.
    pub fn slot_terminate_sync(&mut self) {
        log::info!(target: LOG, "folder  {}  Terminating!", self.alias());
        if self.is_sync_running() {
            if let Some(a) = &self.engine_abort {
                a.abort();
            }
            self.set_sync_state(SyncStatus::SyncAbortRequested);
        }
    }

    /// `wipeForRemoval()`.
    pub fn wipe_for_removal(&mut self) {
        self.disconnect_folder_watcher();
        // Delete files that have been partially downloaded.
        self.slot_discard_download_progress();
        // Close the sync journal.  Do NOT call any methods that fetch data from it
        // after this point, otherwise the journal is re-opened.
        self.journal.close();
        if !std::path::Path::new(self.path()).is_dir() {
            log::error!(target: LOG, "db files are not going to be deleted, sync folder could not be found at {}", self.path());
            return;
        }
        // Remove db and temporaries
        let state_db_file = self
            .journal
            .database_file_path()
            .to_string_lossy()
            .into_owned();
        if std::path::Path::new(&state_db_file).exists() {
            match std::fs::remove_file(&state_db_file) {
                Ok(()) => log::info!(target: LOG, "wipe: Removed csync StateDB  {state_db_file}"),
                Err(_) => {
                    log::warn!(target: LOG, "Failed to remove existing csync StateDB  {state_db_file}")
                }
            }
        } else {
            log::warn!(target: LOG, "statedb is empty, can not remove.");
        }
        // Also remove other db related files
        for suffix in [".ctmp", "-shm", "-wal", "-journal"] {
            let _ = std::fs::remove_file(format!("{state_db_file}{suffix}"));
        }
    }

    /// The part of `FolderMan::unloadFolder` this port needs once the
    /// folder's engine is back: no more watcher, and the journal is closed
    /// (and left on disk) so another client can open it.
    pub fn unload(&mut self) {
        self.disconnect_folder_watcher();
        self.journal.close();
    }

    /// `FolderStatusModel::slotApplySelectiveSync` for this folder: edits the
    /// selective sync lists of the journal (see [`crate::selective_sync`]);
    /// when something changed, a running sync is terminated and the changed
    /// paths are scheduled for local discovery. The caller then schedules
    /// the folder for an immediate sync when [`SelectiveSyncChange::changes`]
    /// is not empty.
    ///
    /// [`SelectiveSyncChange::changes`]: crate::selective_sync::SelectiveSyncChange::changes
    pub fn apply_selective_sync(
        &mut self,
        exclude: &[String],
        include: &[String],
    ) -> Result<crate::selective_sync::SelectiveSyncChange, crate::selective_sync::SelectiveSyncError>
    {
        let change = crate::selective_sync::apply(&self.journal, exclude, include)?;
        // do the sync if there were changes
        if !change.changes.is_empty() {
            if self.is_busy() {
                self.slot_terminate_sync();
            }
            for it in &change.changes {
                self.schedule_path_for_local_discovery(it);
            }
        }
        Ok(change)
    }

    /// `initializeSyncOptions()`.
    fn initialize_sync_options(&self) -> SyncOptions {
        let mut opt = SyncOptions::default();
        let new_folder_limit = self.settings.new_big_folder_size_limit;
        opt.new_big_folder_size_limit = if new_folder_limit.0 {
            new_folder_limit.1 * 1000 * 1000
        } else {
            -1
        }; // convert from MB to B
        opt.confirm_external_storage = self.settings.confirm_external_storage;
        opt.move_files_to_trash = self.settings.move_to_trash;
        opt.notify_existing_folders_over_limit = self.settings.notify_existing_folders_over_limit;
        opt.minimum_file_age_for_upload = self.settings.minimum_file_age_for_upload;
        let caps = self.account.capabilities();
        let caps_max_concurrent_chunk_uploads = caps.max_concurrent_chunk_uploads();
        opt.parallel_network_jobs = if caps_max_concurrent_chunk_uploads > 0 {
            caps_max_concurrent_chunk_uploads
        } else {
            // isHttp2Supported() ? 20 : 6; HTTP/2 support is not tracked by
            // the account here.
            6
        };
        // Chunk V2: Size of chunks must be between 5MB and 5GB, except for the last chunk which can be smaller
        let cfg_min_chunk_size = self.settings.min_chunk_size;
        opt.set_min_chunk_size(cfg_min_chunk_size);
        let caps_max_chunk_size = caps.max_chunk_size();
        if caps_max_chunk_size > 0 {
            opt.set_max_chunk_size(caps_max_chunk_size);
            opt.initial_chunk_size = caps_max_chunk_size;
        } else {
            let cfg_max_chunk_size = self.settings.max_chunk_size;
            opt.set_max_chunk_size(cfg_max_chunk_size);
            opt.initial_chunk_size = self.settings.chunk_size.clamp(
                cfg_min_chunk_size,
                cfg_max_chunk_size.max(cfg_min_chunk_size),
            );
        }
        opt.fill_from_environment_variables();
        opt.verify_chunk_sizes();
        opt
    }

    /// `startSync()`.
    pub fn start_sync(&mut self) -> Vec<FolderAction> {
        self.silence_errors_until_next_sync = false;
        if self.is_busy() {
            log::error!(target: LOG, "ERROR csync is still running and new sync requested.");
            return Vec::new();
        }
        self.time_since_last_sync_start = Instant::now();
        self.sync_result.set_status(SyncStatus::SyncPrepare);
        let mut actions = vec![FolderAction::SyncStateChange];
        log::info!(target: LOG, "*** Start syncing  {}  - ncsyncd client version {}", self.remote_url(), env!("CARGO_PKG_VERSION"));
        let options = self.initialize_sync_options();
        let reliable_watcher = self
            .folder_watcher
            .as_ref()
            .is_some_and(|w| w.is_reliable());
        let can_set_permissions = self
            .folder_watcher
            .as_ref()
            .is_some_and(|w| w.can_set_permissions());
        let Some(mut engine) = self.engine.take() else {
            return actions;
        };
        if !engine.excluded_files().reload_exclude_files() {
            self.engine = Some(engine);
            self.slot_sync_error("Could not read system exclude file");
            // QMetaObject::invokeMethod(this, "slotSyncFinished", Qt::QueuedConnection, Q_ARG(bool, false))
            let _ = self.tx.send(Event::Folder(
                self.id,
                FolderEvent::SyncError(String::new(), ErrorCategory::GenericError),
            ));
            actions.extend(self.slot_sync_finished_without_engine());
            return actions;
        }
        let (up, down) = self.network_limits.engine_limits();
        engine.set_network_limits(up, down);
        engine.set_sync_options(options);
        let has_done_full_local_discovery = self.time_since_last_full_local_discovery.is_some();
        let periodic_full_local_discovery_now = match self.settings.full_local_discovery_interval {
            // negative means we don't require periodic full runs
            None => false,
            Some(interval) => self
                .time_since_last_full_local_discovery
                .is_none_or(|t| t.elapsed() > interval),
        };
        if reliable_watcher && has_done_full_local_discovery && !periodic_full_local_discovery_now {
            log::info!(target: LOG, "Allowing local discovery to read from the database");
            engine.set_local_discovery_options(
                LocalDiscoveryStyle::DatabaseAndFilesystem,
                self.local_discovery_tracker
                    .local_discovery_paths()
                    .iter()
                    .cloned(),
            );
            self.local_discovery_tracker.start_sync_partial_discovery();
        } else {
            log::info!(target: LOG, "Forbidding local discovery to read from the database");
            engine.set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, Vec::new());
            self.local_discovery_tracker.start_sync_full_discovery();
        }
        engine.set_ignore_hidden_files(self.definition.ignore_hidden_files);
        engine.set_filesystem_permissions_reliable(can_set_permissions);
        self.engine_abort = Some(engine.abort_handle());
        self.progress = None;
        // QMetaObject::invokeMethod(_engine.data(), "startSync", Qt::QueuedConnection)
        let tx = self.tx.clone();
        let id = self.id;
        tokio::task::spawn_local(async move {
            let mut engine = engine;
            let success = engine.sync_once().await;
            let _ = tx.send(Event::Folder(
                id,
                FolderEvent::EngineFinished(Box::new(engine), success),
            ));
        });
        actions.push(FolderAction::SyncStarted);
        actions
    }

    /// `slotSyncError(message, category)`.
    pub fn slot_sync_error(&mut self, message: &str) {
        if message.is_empty() {
            return;
        }
        if !self.silence_errors_until_next_sync {
            self.sync_result.append_error_string(message);
        }
    }

    /// `slotSyncStarted()`.
    pub fn slot_sync_started(&mut self) -> Vec<FolderAction> {
        log::info!(target: LOG, "#### Propagation start ####################################################");
        self.sync_result.set_status(SyncStatus::SyncRunning);
        vec![FolderAction::SyncStateChange]
    }

    /// `slotItemCompleted(item, category)` and
    /// `LocalDiscoveryTracker::slotItemCompleted`.
    pub fn slot_item_completed(&mut self, item: &SyncFileItemPtr) {
        self.local_discovery_tracker
            .slot_item_completed(&item.borrow());
        let mut item = item.borrow_mut();
        if item.instruction == Instruction::None || item.instruction == Instruction::UpdateMetadata
        {
            // We only care about the updates that deserve to be shown in the UI
            return;
        }
        if self.silence_errors_until_next_sync
            && item.status != ItemStatus::Success
            && item.status != ItemStatus::NoStatus
        {
            item.error_string.clear();
            item.status = ItemStatus::SoftError;
        }
        self.sync_result.process_completed_item(&item);
    }

    /// `slotTransmissionProgress(progress)`.
    pub fn slot_transmission_progress(&mut self, pi: ProgressInfo) {
        self.progress = Some(pi);
    }

    /// The engine came back: `SyncEngine::finished(success)`.
    pub fn engine_finished(&mut self, engine: SyncEngine, success: bool) -> Vec<FolderAction> {
        self.engine = Some(engine);
        self.engine_abort = None;
        // LocalDiscoveryTracker::slotSyncFinished (connected to the engine)
        self.local_discovery_tracker.slot_sync_finished(success);
        self.arm_scheduled_sync_timer();
        self.slot_sync_finished(success)
    }

    fn slot_sync_finished_without_engine(&mut self) -> Vec<FolderAction> {
        self.slot_sync_finished(false)
    }

    /// `slotSyncFinished(success)`.
    fn slot_sync_finished(&mut self, success: bool) -> Vec<FolderAction> {
        log::info!(target: LOG, "Client version ncsyncd {}", env!("CARGO_PKG_VERSION"));
        let sync_error = !self.sync_result.error_strings().is_empty();
        if sync_error {
            log::warn!(target: LOG, "SyncEngine finished with ERROR");
        } else {
            log::info!(target: LOG, "SyncEngine finished without problem.");
        }
        self.show_sync_result_popup();
        let another_sync_needed = self.another_sync_needed();
        if sync_error {
            self.sync_result.set_status(SyncStatus::Error);
            if self.silence_errors_until_next_sync {
                self.sync_result.set_status(SyncStatus::Success);
            }
        } else if self.sync_result.found_files_not_synced() {
            self.sync_result.set_status(SyncStatus::Problem);
        } else if self.definition.paused {
            // Maybe the sync was terminated because the user paused the folder
            self.sync_result.set_status(SyncStatus::Paused);
        } else {
            self.sync_result.set_status(SyncStatus::Success);
        }
        let ok_status = matches!(
            self.sync_result.status(),
            SyncStatus::Success | SyncStatus::Problem
        );
        // Count the number of syncs that have failed in a row.
        if ok_status {
            self.consecutive_failing_syncs = 0;
        } else {
            self.consecutive_failing_syncs += 1;
            log::info!(target: LOG, "the last {} syncs failed", self.consecutive_failing_syncs);
        }
        if ok_status
            && success
            && self.engine.as_ref().is_some_and(|e| {
                e.last_local_discovery_style() == LocalDiscoveryStyle::FilesystemOnly
            })
        {
            self.time_since_last_full_local_discovery = Some(Instant::now());
        }
        // The syncFinished result that is to be triggered here makes the folderman
        // clear the current running sync folder marker.
        // Lets wait a bit to do that because, as long as this marker is not cleared,
        // file system change notifications are ignored for that folder. And it takes
        // some time under certain conditions to make the file system notifications
        // all come in.
        single_shot(
            Duration::from_millis(200),
            &self.tx,
            Event::Folder(self.id, FolderEvent::EmitFinishedDelayed),
        );
        self.last_sync_duration = self.time_since_last_sync_start.elapsed();
        self.time_since_last_sync_done = Instant::now();
        // Increment the follow-up sync counter if necessary.
        if another_sync_needed == AnotherSyncNeeded::ImmediateFollowUp {
            self.consecutive_follow_up_syncs += 1;
            log::info!(target: LOG, "another sync was requested by the finished sync, this has happened {} times", self.consecutive_follow_up_syncs);
        } else {
            self.consecutive_follow_up_syncs = 0;
        }
        // Maybe force a follow-up sync to take place, but only a couple of times.
        if another_sync_needed == AnotherSyncNeeded::ImmediateFollowUp
            && self.consecutive_follow_up_syncs <= 3
        {
            // Sometimes another sync is requested because a local file is still
            // changing, so wait at least a small amount of time before syncing
            // the folder again.
            self.schedule_this_folder_soon();
        }
        vec![FolderAction::SyncStateChange]
    }

    /// `showSyncResultPopup()`: the summary log line (the popups are GUI).
    fn show_sync_result_popup(&self) {
        let r = &self.sync_result;
        if let Some(i) = r.first_item_new() {
            log::info!(target: LOG, "{} and {} other file(s) have been added.", i.destination, r.num_new_items() - 1);
        }
        if let Some(i) = r.first_item_deleted() {
            log::info!(target: LOG, "{} and {} other file(s) have been removed.", i.destination, r.num_removed_items() - 1);
        }
        if let Some(i) = r.first_item_updated() {
            log::info!(target: LOG, "{} and {} other file(s) have been updated.", i.destination, r.num_updated_items() - 1);
        }
        if let Some(i) = r.first_item_renamed() {
            log::info!(target: LOG, "{} has been renamed to {} and {} other file(s) have been renamed.", i.file, i.rename_target, r.num_renamed_items() - 1);
        }
        if let Some(i) = r.first_new_conflict_item() {
            log::warn!(target: LOG, "{} and {} other file(s) have sync conflicts.", i.destination, r.num_new_conflict_items() - 1);
        }
        if r.num_error_items() > 0
            && let Some(i) = r.first_item_error()
        {
            log::warn!(target: LOG, "{} and {} other file(s) could not be synced due to errors. See the log for details.", i.file, r.num_error_items() - 1);
        }
        if r.num_locked_items() > 0
            && let Some(i) = r.first_item_locked()
        {
            log::warn!(target: LOG, "{} and {} other file(s) are currently locked.", i.file, r.num_locked_items() - 1);
        }
        log::info!(target: LOG, "Folder {} sync result:  {}", r.folder(), r.status().name());
    }

    /// `slotEmitFinishedDelayed()`.
    pub fn slot_emit_finished_delayed(&mut self, account_connected: bool) -> Vec<FolderAction> {
        let mut actions = vec![FolderAction::SyncFinished];
        // Immediately check the etag again if there was some sync activity.
        let r = &self.sync_result;
        if matches!(r.status(), SyncStatus::Success | SyncStatus::Problem)
            && (r.first_item_deleted().is_some()
                || r.first_item_new().is_some()
                || r.first_item_renamed().is_some()
                || r.first_item_updated().is_some()
                || r.first_new_conflict_item().is_some())
        {
            actions.extend(self.slot_run_etag_job(account_connected));
        }
        actions
    }

    /// `slotNewBigFolderDiscovered(newF, isExternal)` (the journal part ran
    /// synchronously): the user-facing message.
    pub fn new_big_folder_message(&self, new_f: &str, is_external: bool) {
        if is_external {
            log::warn!(target: LOG, "A folder from an external storage has been added. Please go in the settings to select it if you wish to download it. ({new_f})");
        } else {
            log::warn!(target: LOG, "A new folder larger than {} MB has been added: {new_f}. Please go in the settings to select it if you wish to download it.", self.settings.new_big_folder_size_limit.1);
        }
    }

    /// `slotExistingFolderNowBig(folderPath)`.
    pub fn slot_existing_folder_now_big(&mut self, folder_path: &str) {
        let trail = nc_journal::utility::trailing_slash_path(folder_path);
        let stop_syncing = self.settings.stop_syncing_existing_folders_over_limit;
        // Add the entry to the whitelist if it is neither in the blacklist or whitelist already
        let black = self
            .journal
            .get_selective_sync_list(SelectiveSyncListType::BlackList);
        let white = self
            .journal
            .get_selective_sync_list(SelectiveSyncListType::WhiteList);
        let in_decided_lists = black.as_ref().is_ok_and(|b| b.contains(&trail))
            || white.as_ref().is_ok_and(|w| w.contains(&trail));
        if in_decided_lists {
            return;
        }
        if let (Ok(black), Ok(white)) = (black, white) {
            let (mut relevant, ty) = if stop_syncing {
                (black, SelectiveSyncListType::BlackList)
            } else {
                (white, SelectiveSyncListType::WhiteList)
            };
            relevant.push(trail.clone());
            self.journal.set_selective_sync_list(ty, &relevant);
            if stop_syncing {
                // Abort current down sync and start again
                self.slot_terminate_sync();
                self.schedule_this_folder_soon();
            }
        }
        if let Ok(mut undecided) = self
            .journal
            .get_selective_sync_list(SelectiveSyncListType::UndecidedList)
        {
            if !undecided.contains(&trail) {
                undecided.push(trail.clone());
                self.journal
                    .set_selective_sync_list(SelectiveSyncListType::UndecidedList, &undecided);
            }
            let instruction = if stop_syncing {
                "Synchronisation of this folder has been disabled."
            } else {
                "Synchronisation of this folder can be disabled in the settings window."
            };
            log::warn!(target: LOG, "A folder has surpassed the set folder size limit of {}MB: {folder_path}.\n{instruction}", self.settings.new_big_folder_size_limit.1);
        }
    }

    /// `slotAboutToRemoveAllFiles` answered "Restore Files": the journal's
    /// file table is cleared so the next sync restores everything.
    pub fn all_files_deleted_restore(&mut self) -> Vec<FolderAction> {
        // FileSystem::setFolderMinimumPermissions(path()): macOS only.
        self.journal.clear_file_table();
        self.last_etag.clear();
        vec![FolderAction::Schedule]
    }

    /// `slotNextSyncFullLocalDiscovery()`.
    pub fn slot_next_sync_full_local_discovery(&mut self) {
        self.time_since_last_full_local_discovery = None;
    }

    /// `setSilenceErrorsUntilNextSync(silenceErrors)`.
    pub fn set_silence_errors_until_next_sync(&mut self, silence: bool) {
        self.silence_errors_until_next_sync = silence;
    }

    /// `schedulePathForLocalDiscovery(relativePath)`.
    pub fn schedule_path_for_local_discovery(&mut self, relative_path: &str) {
        self.local_discovery_tracker.add_touched_path(relative_path);
    }

    /// `scheduleThisFolderSoon()`: always restart self-scheduling timer,
    /// there might still be file changes incoming.
    pub fn schedule_this_folder_soon(&mut self) {
        let id = self.id;
        self.schedule_self_timer.start(&self.tx, move |g| {
            Event::Folder(id, FolderEvent::ScheduleSelfTimer(g))
        });
    }

    /// `_scheduleSelfTimer` timeout: whether it is current.
    pub fn schedule_self_timer_fired(&mut self, generation: u64) -> bool {
        self.schedule_self_timer.fired(generation)
    }

    /// Arms the timer of the engine's earliest scheduled sync run.
    fn arm_scheduled_sync_timer(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        match engine.scheduled_sync_timers().next_deadline() {
            Some(deadline) => {
                let delay = deadline.saturating_duration_since(Instant::now());
                let id = self.id;
                self.scheduled_sync_timer
                    .start_with(delay, &self.tx, move |g| {
                        Event::Folder(id, FolderEvent::ScheduledSyncTimer(g))
                    });
            }
            None => self.scheduled_sync_timer.stop(),
        }
    }

    /// The scheduled sync run timer fired: `startSync()` of the engine
    /// directly, like the timer's lambda upstream (only when idle).
    pub fn scheduled_sync_timer_fired(&mut self, generation: u64) -> bool {
        if !self.scheduled_sync_timer.fired(generation) {
            return false;
        }
        let Some(engine) = self.engine.as_mut() else {
            // The running sync rediscovers the files anyway.
            return false;
        };
        let fire = engine.scheduled_sync_timers().fire_due(Instant::now());
        self.arm_scheduled_sync_timer();
        fire
    }

    /// `acceptInvalidFileName(filePath)`.
    pub fn accept_invalid_file_name(&mut self, file_path: &str) {
        if let Some(e) = self.engine.as_mut() {
            e.add_accepted_invalid_file_name(file_path);
        }
    }

    /// `hasFileIds(fileIds)`: whether the current folder contains any of
    /// the passed fileIds.
    pub fn has_file_ids(&self, file_ids: &[i64]) -> bool {
        file_ids.contains(&self.root_file_id) || self.journal.has_file_ids(file_ids)
    }

    /// `whitelistPath(path)`.
    pub fn whitelist_path(&self, path: &str) {
        self.remove_path_from_selective_sync_list(path, SelectiveSyncListType::UndecidedList);
        self.remove_path_from_selective_sync_list(path, SelectiveSyncListType::BlackList);
        self.append_path_to_selective_sync_list(path, SelectiveSyncListType::WhiteList);
    }

    /// `blacklistPath(path)`.
    pub fn blacklist_path(&self, path: &str) {
        self.remove_path_from_selective_sync_list(path, SelectiveSyncListType::UndecidedList);
        self.remove_path_from_selective_sync_list(path, SelectiveSyncListType::WhiteList);
        self.append_path_to_selective_sync_list(path, SelectiveSyncListType::BlackList);
    }

    fn append_path_to_selective_sync_list(&self, path: &str, list_type: SelectiveSyncListType) {
        let folder_path = nc_journal::utility::trailing_slash_path(path);
        if let Ok(mut list) = self.journal.get_selective_sync_list(list_type) {
            list.push(folder_path);
            self.journal.set_selective_sync_list(list_type, &list);
        }
    }

    fn remove_path_from_selective_sync_list(&self, path: &str, list_type: SelectiveSyncListType) {
        let folder_path = nc_journal::utility::trailing_slash_path(path);
        if let Ok(mut list) = self.journal.get_selective_sync_list(list_type) {
            list.retain(|p| *p != folder_path);
            self.journal.set_selective_sync_list(list_type, &list);
        }
    }
}

/// `Folder::pathIsIgnored(path)`.
fn path_is_ignored(
    excluded: &Rc<RefCell<ExcludedFiles>>,
    base: &str,
    exclude_hidden: bool,
    path: &str,
) -> bool {
    if path.is_empty() {
        return true;
    }
    if excluded.borrow().is_excluded(path, base, exclude_hidden)
        && !nc_journal::utility::is_conflict_file(path)
    {
        log::debug!(target: LOG, "* Ignoring file {path}");
        return true;
    }
    false
}

/// `Folder::slotNewBigFolderDiscovered(newF, isExternal)`: the journal
/// part.
fn slot_new_big_folder_discovered(
    journal: &SyncJournalDb,
    new_f: &str,
    _is_external: bool,
    _limit_mb: i64,
) {
    let new_folder = nc_journal::utility::trailing_slash_path(new_f);
    // Add the entry to the blacklist if it is neither in the blacklist or whitelist already
    if let (Ok(mut blacklist), Ok(whitelist)) = (
        journal.get_selective_sync_list(SelectiveSyncListType::BlackList),
        journal.get_selective_sync_list(SelectiveSyncListType::WhiteList),
    ) && !blacklist.contains(&new_folder)
        && !whitelist.contains(&new_folder)
    {
        blacklist.push(new_folder.clone());
        journal.set_selective_sync_list(SelectiveSyncListType::BlackList, &blacklist);
    }
    // And add the entry to the undecided list and signal the UI
    if let Ok(mut undecided) = journal.get_selective_sync_list(SelectiveSyncListType::UndecidedList)
        && !undecided.contains(&new_folder)
    {
        undecided.push(new_folder);
        journal.set_selective_sync_list(SelectiveSyncListType::UndecidedList, &undecided);
    }
}

/// `FolderDefinition::prepareTargetPath(path)`: remove ending /, then
/// ensure starting '/': so "/foo/bar" and "/".
pub fn prepare_target_path(path: &str) -> String {
    let p = path.strip_suffix('/').unwrap_or(path);
    // Doing this second ensures the empty string or "/" come
    // out as "/".
    if p.starts_with('/') {
        p.to_owned()
    } else {
        format!("/{p}")
    }
}
