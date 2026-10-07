// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/syncengine.{h,cpp}` (nextcloud/desktop
// v34.0.5), without the GUI-only parts (status tracker, progress info,
// scheduled sync timers, virtual files, end-to-end encryption).

//! `SyncEngine`: runs one sync (discovery, reconcile, propagation) of a
//! local folder against a remote folder.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use nc_dav::{Account, JobOptions, Target};
use nc_journal::exclude::ExcludedFiles;
use nc_journal::journal::{
    ConflictRecord, SelectiveSyncListType, SyncJournalDb, SyncJournalFileLockInfo,
};
use nc_journal::remote_permissions::Permission;
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::discovery::{
    DiscoveryError, DiscoveryPhase, LocalDiscoveryStyle, PathTuple, QueryMode, RootJobSpec,
};
use crate::filesystem;
use crate::item::{
    Direction, ErrorCategory, Instruction, Status, SyncFileItemPtr, SyncFileItemVector,
    item_ordering,
};
use crate::options::SyncOptions;
use crate::propagator::{AbortHandle, Propagator, PropagatorEvent};

const LOG: &str = "nextcloud.sync.engine";

/// `AnotherSyncNeeded`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnotherSyncNeeded {
    #[default]
    NoFollowUpSync,
    /// schedule this again immediately (limited amount of times)
    ImmediateFollowUp,
    /// regularly schedule this folder again (around 1/minute, unlimited)
    DelayedFollowUp,
}

/// The signals of the sync engine.
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub struct EngineCallbacks {
    /// `itemDiscovered(item)`.
    pub item_discovered: Option<Box<dyn FnMut(&SyncFileItemPtr)>>,
    /// `itemCompleted(item, category)`.
    pub item_completed: Option<Box<dyn FnMut(&SyncFileItemPtr, ErrorCategory)>>,
    /// `aboutToPropagate(items)`.
    pub about_to_propagate: Option<Box<dyn FnMut(&SyncFileItemVector)>>,
    /// `aboutToRemoveAllFiles(direction, callback)`: return true to cancel
    /// the sync, false to continue.
    pub about_to_remove_all_files: Option<Box<dyn FnMut(Direction) -> bool>>,
    /// `newBigFolder(folder, isExternal)`.
    pub new_big_folder: Option<Box<dyn FnMut(&str, bool)>>,
    /// `existingFolderNowBig(folder)`.
    pub existing_folder_now_big: Option<Box<dyn FnMut(&str)>>,
    /// `syncError(message, category)`.
    pub sync_error: Option<Box<dyn FnMut(&str, ErrorCategory)>>,
    /// `started()`.
    pub started: Option<Box<dyn FnMut()>>,
    /// `finished(success)`.
    pub finished: Option<Box<dyn FnMut(bool)>>,
    /// `seenLockedFile(fileName)`.
    pub seen_locked_file: Option<Box<dyn FnMut(&str)>>,
    /// `rootFileIdReceived(fileId)`.
    pub root_file_id_received: Option<Box<dyn FnMut(i64)>>,
    /// `transmissionProgress`, simplified: (file, bytes) of completed items.
    pub propagator_event: Option<Box<dyn FnMut(&PropagatorEvent)>>,
}

/// State shared with the discovery and propagator callbacks.
struct SyncState {
    sync_items: SyncFileItemVector,
    has_none_files: bool,
    has_remove_file: bool,
    needs_update: bool,
    seen_conflict_files: HashSet<String>,
    remnant_read_only_folders: SyncFileItemVector,
    unique_errors: HashSet<String>,
}

/// Aborts a running sync from a callback (`SyncEngine::abort`).
#[derive(Clone)]
pub struct EngineAbortHandle {
    discovery: CancellationToken,
    propagator: Rc<RefCell<Option<AbortHandle>>>,
}

impl EngineAbortHandle {
    pub fn abort(&self) {
        if let Some(p) = self.propagator.borrow().as_ref() {
            // If we're already in the propagation phase, aborting that is sufficient
            p.abort();
        } else {
            self.discovery.cancel();
        }
    }
}

/// The sync engine (`SyncEngine`).
pub struct SyncEngine {
    account: Arc<Account>,
    /// Ends with '/'.
    local_path: String,
    remote_path: String,
    journal: Arc<SyncJournalDb>,
    sync_options: SyncOptions,
    excluded_files: Rc<RefCell<ExcludedFiles>>,
    ignore_hidden_files: bool,
    local_discovery_style: LocalDiscoveryStyle,
    local_discovery_paths: Vec<String>,
    last_local_discovery_style: LocalDiscoveryStyle,
    another_sync_needed: AnotherSyncNeeded,
    upload_limit: i32,
    download_limit: i32,
    leading_and_trailing_spaces_files_allowed: Vec<String>,
    should_enforce_windows_file_name_compatibility: bool,
    filesystem_permissions_reliable: bool,
    remote_root_etag: Vec<u8>,
    /// `_rootFileIdReceived` / `_rootFileId`: set once for the engine's
    /// lifetime, so the signal is emitted only on the first sync.
    root_file_id: Rc<std::cell::Cell<Option<i64>>>,
    /// `ConfigFile::promptDeleteFiles()` (default false).
    pub prompt_delete_files: bool,
    /// `ConfigFile::deleteFilesThreshold()` (default 100).
    pub delete_files_threshold: i64,
    callbacks: Rc<RefCell<EngineCallbacks>>,
    discovery_abort: CancellationToken,
    propagator_abort: Rc<RefCell<Option<AbortHandle>>>,
}

fn emit_sync_error(cbs: &Rc<RefCell<EngineCallbacks>>, msg: &str, cat: ErrorCategory) {
    log::warn!(target: LOG, "Sync error: {msg}");
    if let Some(cb) = cbs.borrow_mut().sync_error.as_mut() {
        cb(msg, cat);
    }
}

impl SyncEngine {
    /// `SyncEngine(account, localPath, syncOptions, remotePath, journal)`.
    /// `local_path` must end with '/'.
    pub fn new(
        account: Arc<Account>,
        local_path: &str,
        sync_options: SyncOptions,
        remote_path: &str,
        journal: Arc<SyncJournalDb>,
    ) -> Self {
        debug_assert!(local_path.ends_with('/'));
        Self {
            account,
            local_path: local_path.to_owned(),
            remote_path: remote_path.to_owned(),
            journal,
            sync_options,
            excluded_files: Rc::new(RefCell::new(ExcludedFiles::new(local_path))),
            ignore_hidden_files: false,
            local_discovery_style: LocalDiscoveryStyle::FilesystemOnly,
            local_discovery_paths: Vec::new(),
            last_local_discovery_style: LocalDiscoveryStyle::FilesystemOnly,
            another_sync_needed: AnotherSyncNeeded::NoFollowUpSync,
            upload_limit: 0,
            download_limit: 0,
            leading_and_trailing_spaces_files_allowed: Vec::new(),
            should_enforce_windows_file_name_compatibility: false,
            filesystem_permissions_reliable: false,
            remote_root_etag: Vec::new(),
            root_file_id: Rc::new(std::cell::Cell::new(None)),
            prompt_delete_files: false,
            delete_files_threshold: 100,
            callbacks: Rc::new(RefCell::new(EngineCallbacks::default())),
            discovery_abort: CancellationToken::new(),
            propagator_abort: Rc::new(RefCell::new(None)),
        }
    }

    pub fn account(&self) -> &Arc<Account> {
        &self.account
    }

    pub fn journal(&self) -> &Arc<SyncJournalDb> {
        &self.journal
    }

    pub fn local_path(&self) -> &str {
        &self.local_path
    }

    pub fn sync_options(&self) -> &SyncOptions {
        &self.sync_options
    }

    pub fn set_sync_options(&mut self, options: SyncOptions) {
        self.sync_options = options;
    }

    pub fn excluded_files(&self) -> std::cell::RefMut<'_, ExcludedFiles> {
        self.excluded_files.borrow_mut()
    }

    pub fn callbacks(&self) -> std::cell::RefMut<'_, EngineCallbacks> {
        self.callbacks.borrow_mut()
    }

    pub fn set_ignore_hidden_files(&mut self, ignore: bool) {
        self.ignore_hidden_files = ignore;
    }

    pub fn ignore_hidden_files(&self) -> bool {
        self.ignore_hidden_files
    }

    pub fn set_network_limits(&mut self, upload: i32, download: i32) {
        self.upload_limit = upload;
        self.download_limit = download;
    }

    /// `isAnotherSyncNeeded()`.
    pub fn is_another_sync_needed(&self) -> AnotherSyncNeeded {
        self.another_sync_needed
    }

    pub fn last_local_discovery_style(&self) -> LocalDiscoveryStyle {
        self.last_local_discovery_style
    }

    /// `addAcceptedInvalidFileName`.
    pub fn add_accepted_invalid_file_name(&mut self, file_path: &str) {
        self.leading_and_trailing_spaces_files_allowed
            .push(file_path.to_owned());
    }

    /// `setLocalDiscoveryEnforceWindowsFileNameCompatibility`.
    pub fn set_local_discovery_enforce_windows_file_name_compatibility(&mut self, value: bool) {
        self.should_enforce_windows_file_name_compatibility = value;
    }

    /// `setFilesystemPermissionsReliable`.
    pub fn set_filesystem_permissions_reliable(&mut self, reliable: bool) {
        self.filesystem_permissions_reliable = reliable;
    }

    /// The handle to abort the current sync from a callback.
    pub fn abort_handle(&self) -> EngineAbortHandle {
        EngineAbortHandle {
            discovery: self.discovery_abort.clone(),
            propagator: self.propagator_abort.clone(),
        }
    }

    /// `abort()`.
    pub fn abort(&self) {
        self.abort_handle().abort();
    }

    /// `setLocalDiscoveryOptions(style, paths)`: normalizes the paths so that
    /// no path is contained in another.
    pub fn set_local_discovery_options(
        &mut self,
        style: LocalDiscoveryStyle,
        paths: impl IntoIterator<Item = String>,
    ) {
        self.local_discovery_style = style;
        let mut list: Vec<String> = paths.into_iter().collect();
        list.sort_by(|a, b| crate::utility::qstring_cmp(a, b));
        list.dedup();
        // Note: for simplicity, this code consider anything less than '/' as a path separator
        let mut out: Vec<String> = Vec::new();
        let mut prev: Option<String> = None;
        for it in list {
            if let Some(p) = &prev
                && it.starts_with(p.as_str())
                && (p.ends_with('/')
                    || it == *p
                    || it.as_bytes().get(p.len()).is_some_and(|c| *c <= b'/'))
            {
                continue;
            }
            prev = Some(it.clone());
            out.push(it);
        }
        self.local_discovery_paths = out;
    }

    /// `shouldDiscoverLocally(path)`.
    pub fn should_discover_locally(&self, path: &str) -> bool {
        should_discover_locally(
            self.local_discovery_style,
            &self.local_discovery_paths,
            path,
        )
    }

    /// `startSync()` until `finished(success)`: one complete sync run.
    pub async fn sync_once(&mut self) -> bool {
        let success = self.start_sync().await;
        // finalize(success)
        self.local_discovery_paths.clear();
        self.local_discovery_style = LocalDiscoveryStyle::FilesystemOnly;
        self.leading_and_trailing_spaces_files_allowed.clear();
        *self.propagator_abort.borrow_mut() = None;
        self.discovery_abort = CancellationToken::new();
        // connected in the constructor: store the time of the last sync
        self.journal
            .key_value_store_set("last_sync", crate::utility::current_secs_since_epoch());
        if let Some(cb) = self.callbacks.borrow_mut().finished.as_mut() {
            cb(success);
        }
        success
    }

    async fn start_sync(&mut self) -> bool {
        let cbs = self.callbacks.clone();
        if self.journal.exists() {
            let poll_infos = self.journal.get_poll_infos();
            if !poll_infos.is_empty() {
                log::info!(target: LOG, "Finish Poll jobs before starting a sync");
                if let Err(e) = self.cleanup_polls(poll_infos).await {
                    emit_sync_error(&cbs, &e, ErrorCategory::GenericError);
                    return false;
                }
            }
        }
        self.another_sync_needed = AnotherSyncNeeded::NoFollowUpSync;
        let state = Rc::new(RefCell::new(SyncState {
            sync_items: Vec::new(),
            has_none_files: false,
            has_remove_file: false,
            needs_update: false,
            seen_conflict_files: HashSet::new(),
            remnant_read_only_folders: Vec::new(),
            unique_errors: HashSet::new(),
        }));
        if !filesystem::file_exists(&self.local_path) {
            self.another_sync_needed = AnotherSyncNeeded::DelayedFollowUp;
            // No _tr, it should only occur in non-mirall
            emit_sync_error(
                &cbs,
                "Unable to find local sync folder.",
                ErrorCategory::GenericError,
            );
            return false;
        }
        // Check free size on disk first.
        let min_free = crate::propagator::critical_free_space_limit();
        let free_bytes = filesystem::free_disk_space(&self.local_path);
        if free_bytes >= 0 {
            if free_bytes < min_free {
                log::warn!(target: LOG, "Too little space available at {}. Have {free_bytes} bytes and require at least {min_free} bytes", self.local_path);
                self.another_sync_needed = AnotherSyncNeeded::DelayedFollowUp;
                emit_sync_error(
                    &cbs,
                    &format!(
                        "Only {} are available, need at least {} to start",
                        crate::utility::octets_to_string(free_bytes),
                        crate::utility::octets_to_string(min_free)
                    ),
                    ErrorCategory::GenericError,
                );
                return false;
            }
        } else {
            log::warn!(target: LOG, "Could not determine free space available at {}", self.local_path);
        }
        // This creates the DB if it does not exist yet.
        if !self.journal.open() {
            log::warn!(target: LOG, "No way to create a sync journal!");
            emit_sync_error(
                &cbs,
                "Unable to open or create the local sync database. Make sure you have write access in the sync folder.",
                ErrorCategory::GenericError,
            );
            return false;
        }
        // Functionality like selective sync might have set up etag storage
        // filtering via schedulePathForRemoteDiscovery(). This *is* the next sync, so
        // undo the filter to allow this sync to retrieve and store the correct etags.
        self.journal.clear_etag_storage_filter();
        self.excluded_files
            .borrow_mut()
            .set_exclude_conflict_files(!self.account.capabilities().upload_conflict_files());
        self.last_local_discovery_style = self.local_discovery_style;

        let Ok(selective_sync_black_list) = self
            .journal
            .get_selective_sync_list(SelectiveSyncListType::BlackList)
        else {
            log::warn!(target: LOG, "Could not retrieve selective sync list from DB");
            emit_sync_error(
                &cbs,
                "Unable to read the blacklist from the local database",
                ErrorCategory::GenericError,
            );
            return false;
        };
        self.process_case_clash_conflicts_before_discovery();

        let mut discovery = DiscoveryPhase::new(
            self.local_path.clone(),
            nc_journal::utility::trailing_slash_path(&self.remote_path),
            self.journal.clone(),
            self.account.clone(),
            self.sync_options.clone(),
            self.excluded_files.clone(),
        );
        discovery.file_system_reliable_permissions = self.filesystem_permissions_reliable;
        discovery.leading_and_trailing_spaces_files_allowed =
            self.leading_and_trailing_spaces_files_allowed.clone();
        let exclude_file_path = format!("{}.sync-exclude.lst", self.local_path);
        if filesystem::file_exists(&exclude_file_path) {
            let mut ex = self.excluded_files.borrow_mut();
            ex.add_exclude_file_path(&exclude_file_path);
            ex.reload_exclude_files();
        }
        let style = self.local_discovery_style;
        let paths = self.local_discovery_paths.clone();
        discovery.should_discover_locally =
            Box::new(move |p| should_discover_locally(style, &paths, p));
        discovery.set_selective_sync_black_list(selective_sync_black_list);
        let Ok(white) = self
            .journal
            .get_selective_sync_list(SelectiveSyncListType::WhiteList)
        else {
            log::warn!(target: LOG, "Unable to read selective sync list, aborting.");
            emit_sync_error(
                &cbs,
                "Unable to read from the sync journal.",
                ErrorCategory::GenericError,
            );
            return false;
        };
        discovery.set_selective_sync_white_list(white);
        let caps = self.account.capabilities();
        discovery.forbidden_filenames = caps.forbidden_filenames();
        discovery.forbidden_basenames = caps.forbidden_filename_basenames();
        discovery.forbidden_extensions = caps.forbidden_filename_extensions();
        discovery.forbidden_chars = caps.forbidden_filename_characters();
        if !discovery.forbidden_filenames.is_empty()
            && !discovery.forbidden_basenames.is_empty()
            && !discovery.forbidden_extensions.is_empty()
            && !discovery.forbidden_chars.is_empty()
        {
            self.should_enforce_windows_file_name_compatibility = true;
        }
        discovery.should_enforce_windows_file_name_compatibility =
            self.should_enforce_windows_file_name_compatibility;
        // Check for invalid character in old server version
        let mut invalid_filename_pattern = caps.invalid_filename_regex();
        if invalid_filename_pattern.is_none()
            && self.account.server_version_int() < nc_dav::account::make_server_version(8, 1, 0)
        {
            // Server versions older than 8.1 don't support some characters in filenames.
            invalid_filename_pattern = Some(r#"[\\:?*"<>|]"#.to_owned());
        }
        if let Some(p) = invalid_filename_pattern
            && !p.is_empty()
        {
            discovery.invalid_filename_rx = Regex::new(&p).ok();
        }
        discovery.server_blacklisted_files = caps.blacklisted_files();
        discovery.ignore_hidden_files = self.ignore_hidden_files;

        // Signals
        let journal = self.journal.clone();
        let local_path = self.local_path.clone();
        let account = self.account.clone();
        {
            let state = state.clone();
            let cbs2 = cbs.clone();
            discovery.callbacks.item_discovered = Some(Box::new(move |item| {
                slot_item_discovered(&state, &cbs2, &journal, &local_path, &account, item);
            }));
        }
        {
            let cbs2 = cbs.clone();
            discovery.callbacks.new_big_folder = Some(Box::new(move |f, ext| {
                if let Some(cb) = cbs2.borrow_mut().new_big_folder.as_mut() {
                    cb(f, ext);
                }
            }));
        }
        {
            let cbs2 = cbs.clone();
            discovery.callbacks.existing_folder_now_big = Some(Box::new(move |f| {
                if let Some(cb) = cbs2.borrow_mut().existing_folder_now_big.as_mut() {
                    cb(f);
                }
            }));
        }
        {
            let state = state.clone();
            discovery.callbacks.remnant_read_only_folder_discovered = Some(Box::new(move |item| {
                state
                    .borrow_mut()
                    .remnant_read_only_folders
                    .push(item.clone());
            }));
        }
        {
            let cbs2 = cbs.clone();
            discovery.callbacks.seen_locked_file = Some(Box::new(move |f| {
                if let Some(cb) = cbs2.borrow_mut().seen_locked_file.as_mut() {
                    cb(f);
                }
            }));
        }
        let remote_root_etag = Rc::new(RefCell::new(Vec::new()));
        {
            let e = remote_root_etag.clone();
            discovery.callbacks.root_etag = Some(Box::new(move |etag, _| {
                if e.borrow().is_empty() {
                    *e.borrow_mut() = etag.to_vec();
                }
            }));
        }
        {
            let cbs2 = cbs.clone();
            // slotRootFileIdReceived: the flag is an engine member.
            let received = self.root_file_id.clone();
            discovery.callbacks.root_file_id_received = Some(Box::new(move |id| {
                if received.get().is_some() {
                    return;
                }
                received.set(Some(id));
                if let Some(cb) = cbs2.borrow_mut().root_file_id_received.as_mut() {
                    cb(id);
                }
            }));
        }

        let root = discovery.create_root_job(RootJobSpec {
            path: PathTuple::default(),
            dir_item: None,
            query_local: QueryMode::NormalQuery,
            last_sync_timestamp: self.journal.key_value_store_get_int("last_sync", 0),
        });
        let abort = self.discovery_abort.clone();
        let result = discovery.run(root, &abort).await;
        self.remote_root_etag = remote_root_etag.borrow().clone();
        match result {
            Ok(()) => {}
            Err(DiscoveryError::Fatal(msg, cat)) => {
                emit_sync_error(&cbs, &msg, cat);
                return false;
            }
            Err(DiscoveryError::Aborted) => return false,
        }
        // slotDiscoveryFinished
        if !self.journal.open() {
            log::warn!(target: LOG, "Bailing out, DB failure");
            emit_sync_error(
                &cbs,
                "Cannot open the sync journal",
                ErrorCategory::GenericError,
            );
            return false;
        }
        // Commits a possibly existing (should not though) transaction and starts a new one for the propagate phase
        self.journal
            .commit_if_needed_and_start_new_transaction("Post discovery");
        // (shouldRestartSync is only relevant with the Windows cfapi VFS)
        if self.handle_mass_deletion(&state) {
            return false;
        }
        if !state.borrow().remnant_read_only_folders.is_empty() {
            self.handle_remnant_read_only_folders(&state);
        }
        self.finish_sync(discovery, state).await
    }

    /// `handleMassDeletion()`: returns true when the sync was cancelled.
    fn handle_mass_deletion(&mut self, state: &Rc<RefCell<SyncState>>) -> bool {
        let display_dialog = self.prompt_delete_files && !self.sync_options.is_cmd();
        let (all_files_deleted, items) = {
            let s = state.borrow();
            (!s.has_none_files && s.has_remove_file, s.sync_items.clone())
        };
        let mut deletion_counter = 0i64;
        for one_item in &items {
            let i = one_item.borrow();
            if i.instruction == Instruction::Remove {
                if i.is_directory() {
                    let _ = self.journal.list_files_in_path(i.file.as_bytes(), |r| {
                        if r.is_file() {
                            deletion_counter += 1;
                        }
                    });
                } else {
                    deletion_counter += 1;
                }
            }
        }
        let files_deleted_threshold_exceeded = deletion_counter > self.delete_files_threshold;
        if (all_files_deleted || files_deleted_threshold_exceeded) && display_dialog {
            log::warn!(target: LOG, "Many files are going to be deleted, asking the user");
            let mut side = 0i64; // > 0 means more deleted on the server.  < 0 means more deleted on the client
            for it in &items {
                let i = it.borrow();
                if i.instruction == Instruction::Remove {
                    side += if i.direction == Direction::Down {
                        1
                    } else {
                        -1
                    };
                }
            }
            let direction = if side >= 0 {
                Direction::Down
            } else {
                Direction::Up
            };
            let cancel = match self
                .callbacks
                .borrow_mut()
                .about_to_remove_all_files
                .as_mut()
            {
                Some(cb) => cb(direction),
                None => false,
            };
            // cancelSyncOrContinue(cancel)
            return cancel;
        }
        false
    }

    /// `handleRemnantReadOnlyFolders()`.
    fn handle_remnant_read_only_folders(&self, state: &Rc<RefCell<SyncState>>) {
        let folders = state.borrow().remnant_read_only_folders.clone();
        for one_folder in folders {
            let (file, item_type) = {
                let f = one_folder.borrow();
                (f.file.clone(), f.item_type)
            };
            let path = format!("{}{}", self.local_path, file);
            let parent = filesystem::parent_dir(&path);
            let _restore = filesystem::FilePermissionsRestore::new(
                &parent,
                filesystem::FolderPermissions::ReadWrite,
            );
            if item_type == nc_journal::csync::ItemType::Directory {
                let mut errors = Vec::new();
                filesystem::remove_recursively(&path, &mut |_, _| {}, &mut errors);
            } else {
                let _ = filesystem::remove(&path);
            }
        }
    }

    /// `processCaseClashConflictsBeforeDiscovery()`.
    fn process_case_clash_conflicts_before_discovery(&self) {
        let mut paths_to_append: HashSet<Vec<u8>> = HashSet::new();
        for p in self.journal.case_clash_conflict_record_paths() {
            let mut split: Vec<&[u8]> = p.split(|c| *c == b'/').collect();
            if split.len() > 1 {
                split.pop();
                paths_to_append.insert(split.join(&b'/'));
            }
        }
        for p in paths_to_append {
            self.journal.schedule_path_for_remote_discovery(&p);
        }
    }

    /// `restoreOldFiles(syncItems)`.
    fn restore_old_files(items: &SyncFileItemVector) {
        // When the server is trying to send us lots of file in the past, this means that a backup
        // was restored in the server.  In that case, we should not simply overwrite the newer file
        // on the file system with the older file from the backup on the server. Instead, we will
        // upload the client file. But we still downloaded the old file in a conflict file just in case
        for sync_item in items {
            let mut i = sync_item.borrow_mut();
            if i.direction != Direction::Down || i.is_selective_sync {
                continue;
            }
            match i.instruction {
                Instruction::Sync => {
                    log::warn!(target: LOG, "restoreOldFiles: RESTORING {}", i.file);
                    i.instruction = Instruction::Conflict;
                }
                Instruction::Remove
                    if i.item_type != nc_journal::csync::ItemType::VirtualFile
                        && i.item_type != nc_journal::csync::ItemType::VirtualFileDownload =>
                {
                    log::warn!(target: LOG, "restoreOldFiles: RESTORING {}", i.file);
                    i.instruction = Instruction::New;
                    i.direction = Direction::Up;
                }
                _ => {}
            }
        }
    }

    /// `finishSync()` + the propagation + `slotPropagationFinished`.
    async fn finish_sync(
        &mut self,
        discovery: DiscoveryPhase,
        state: Rc<RefCell<SyncState>>,
    ) -> bool {
        let cbs = self.callbacks.clone();
        let database_fingerprint = self.journal.data_fingerprint();
        // If databaseFingerprint is empty, this means that there was no information in the database
        // (for example, upgrading from a previous version, or first sync, or server not supporting fingerprint)
        if !database_fingerprint.is_empty() && discovery.data_fingerprint != database_fingerprint {
            log::info!(target: LOG, "data fingerprint changed, assume restore from backup");
            Self::restore_old_files(&state.borrow().sync_items);
        }
        if discovery.another_sync_needed
            && self.another_sync_needed == AnotherSyncNeeded::NoFollowUpSync
        {
            // (slotScheduleFilesDelayedSync: the scheduled sync timers are a daemon feature)
            self.another_sync_needed = AnotherSyncNeeded::ImmediateFollowUp;
        }
        if discovery.has_download_removed_items && discovery.has_upload_error_items {
            self.another_sync_needed = AnotherSyncNeeded::ImmediateFollowUp;
        }
        let items = std::mem::take(&mut state.borrow_mut().sync_items);
        debug_assert!(
            items
                .windows(2)
                .all(|w| item_ordering(&w[0].borrow(), &w[1].borrow()).is_le())
        );
        // To announce the beginning of the sync
        if let Some(cb) = cbs.borrow_mut().about_to_propagate.as_mut() {
            cb(&items);
        }
        // do a database commit
        self.journal.commit("post treewalk", true);

        let mut propagator = Propagator::new(
            self.account.clone(),
            &self.local_path,
            &self.remote_path,
            self.journal.clone(),
            self.sync_options.clone(),
        );
        {
            let cbs2 = cbs.clone();
            propagator.callbacks.item_completed = Some(Box::new(move |item, cat| {
                if let Some(cb) = cbs2.borrow_mut().item_completed.as_mut() {
                    cb(item, cat);
                }
            }));
        }
        {
            let cbs2 = cbs.clone();
            let state2 = state.clone();
            propagator.callbacks.event = Some(Box::new(move |ev| {
                match ev {
                    PropagatorEvent::SeenLockedFile(f) => {
                        if let Some(cb) = cbs2.borrow_mut().seen_locked_file.as_mut() {
                            cb(f);
                        }
                    }
                    PropagatorEvent::InsufficientLocalStorage => {
                        let msg = format!(
                            "Disk space is low: Downloads that would reduce free space below {} were skipped.",
                            crate::utility::octets_to_string(crate::propagator::free_space_limit())
                        );
                        if state2.borrow_mut().unique_errors.insert(msg.clone()) {
                            emit_sync_error(&cbs2, &msg, ErrorCategory::GenericError);
                        }
                    }
                    PropagatorEvent::InsufficientRemoteStorage => {
                        let msg =
                            "There is insufficient space available on the server for some uploads."
                                .to_owned();
                        if state2.borrow_mut().unique_errors.insert(msg.clone()) {
                            emit_sync_error(&cbs2, &msg, ErrorCategory::InsufficientRemoteStorage);
                        }
                    }
                    _ => {}
                }
                if let Some(cb) = cbs2.borrow_mut().propagator_event.as_mut() {
                    cb(ev);
                }
            }));
        }
        // apply the network limits to the propagator
        propagator.set_network_limits(self.upload_limit, self.download_limit);
        self.delete_stale_download_infos(&items);
        self.delete_stale_upload_infos(&items).await;
        self.delete_stale_error_blacklist_entries(&items);
        self.journal.commit("post stale entry removal", true);
        // Emit the started signal only after the propagator has been set up.
        if state.borrow().needs_update
            && let Some(cb) = cbs.borrow_mut().started.as_mut()
        {
            cb();
        }
        *self.propagator_abort.borrow_mut() = Some(propagator.abort_handle());
        propagator.start(items);
        let status = propagator.run().await;

        // slotPropagationFinished
        if propagator.another_sync_needed()
            && self.another_sync_needed == AnotherSyncNeeded::NoFollowUpSync
        {
            self.another_sync_needed = AnotherSyncNeeded::ImmediateFollowUp;
        }
        if status == Status::Success || status == Status::BlacklistedError {
            self.journal
                .set_data_fingerprint(&discovery.data_fingerprint);
        }
        self.conflict_record_maintenance(&state);
        self.case_clash_conflict_record_maintenance();
        self.journal.delete_stale_flags_entries();
        self.journal.commit("All Finished.", false);
        status == Status::Success
    }

    /// `deleteStaleDownloadInfos(syncItems)`.
    fn delete_stale_download_infos(&self, items: &SyncFileItemVector) {
        let mut keep: HashSet<String> = HashSet::new();
        for it in items {
            let i = it.borrow();
            if i.direction == Direction::Down
                && i.item_type == nc_journal::csync::ItemType::File
                && is_file_transfer_instruction(i.instruction)
            {
                keep.insert(i.file.clone());
            }
        }
        for deleted in self.journal.get_and_delete_stale_download_infos(&keep) {
            let tmppath = format!("{}{}", self.local_path, deleted.tmpfile);
            if filesystem::file_exists(&tmppath) {
                let _ = filesystem::remove(&tmppath);
            }
        }
    }

    /// `deleteStaleUploadInfos(syncItems)`: also deletes the stale chunked
    /// uploads on the server.
    async fn delete_stale_upload_infos(&self, items: &SyncFileItemVector) {
        let mut keep: HashSet<String> = HashSet::new();
        for it in items {
            let i = it.borrow();
            if i.direction == Direction::Up
                && i.item_type == nc_journal::csync::ItemType::File
                && is_file_transfer_instruction(i.instruction)
            {
                keep.insert(i.file.clone());
            }
        }
        let ids = self.journal.delete_stale_upload_infos(&keep);
        if self.account.capabilities().chunking_ng() {
            for transfer_id in ids {
                if transfer_id == 0 {
                    continue; // Was not a chunked upload
                }
                let path = format!(
                    "remote.php/dav/uploads/{}/{transfer_id}",
                    self.account.dav_user()
                );
                let _ = nc_dav::jobs::delete(
                    &self.account,
                    &Target::Account(path),
                    &[],
                    false,
                    &JobOptions::default(),
                )
                .await;
            }
        }
    }

    /// `deleteStaleErrorBlacklistEntries(syncItems)`.
    fn delete_stale_error_blacklist_entries(&self, items: &SyncFileItemVector) {
        let mut keep: HashSet<String> = HashSet::new();
        for it in items {
            let i = it.borrow();
            if i.has_blacklist_entry {
                keep.insert(i.file.clone());
            }
        }
        if self
            .journal
            .delete_stale_error_blacklist_entries(&keep)
            .is_err()
        {
            log::warn!(target: LOG, "Could not delete StaleErrorBlacklistEntries from DB");
        }
    }

    /// `conflictRecordMaintenance()`.
    fn conflict_record_maintenance(&self, state: &Rc<RefCell<SyncState>>) {
        // Remove stale conflict entries from the database
        // by checking which files still exist and removing the missing ones.
        let conflict_record_paths = self.journal.conflict_record_paths();
        for path in &conflict_record_paths {
            let fs_path = format!("{}{}", self.local_path, String::from_utf8_lossy(path));
            if !filesystem::file_exists(&fs_path) {
                self.journal.delete_conflict_record(path);
            }
        }
        // Did the sync see any conflict files that don't yet have records?
        // If so, add them now.
        let seen: Vec<String> = state.borrow().seen_conflict_files.iter().cloned().collect();
        for path in seen {
            let bapath = path.as_bytes().to_vec();
            if !conflict_record_paths.contains(&bapath) {
                let base_path = nc_journal::utility::conflict_file_base_name_from_pattern(&bapath);
                let mut record = ConflictRecord {
                    path: bapath,
                    initial_base_path: base_path.clone(),
                    ..ConflictRecord::default()
                };
                // Determine fileid of target file
                if let Ok(Some(base_record)) = self.journal.get_file_record(&base_path)
                    && base_record.is_valid()
                {
                    record.base_file_id = base_record.file_id;
                }
                self.journal.set_conflict_record(&record);
            }
        }
    }

    /// `caseClashConflictRecordMaintenance()`.
    fn case_clash_conflict_record_maintenance(&self) {
        for path in self.journal.case_clash_conflict_record_paths() {
            let fs_path = format!("{}{}", self.local_path, String::from_utf8_lossy(&path));
            if !filesystem::file_exists(&fs_path) {
                self.journal
                    .delete_case_clash_conflict_by_path_record(&String::from_utf8_lossy(&path));
            }
        }
    }

    /// `CleanupPollsJob`: finishes the asynchronous uploads of a previous
    /// sync before starting a new one.
    async fn cleanup_polls(
        &self,
        poll_infos: Vec<nc_journal::journal::PollInfo>,
    ) -> Result<(), String> {
        for info in poll_infos {
            let item = crate::item::new_item();
            {
                let mut i = item.borrow_mut();
                i.file = info.file.clone();
                i.modtime = info.modtime;
                i.size = info.file_size;
            }
            crate::propagator::poll_for_cleanup(
                self.account.clone(),
                self.journal.clone(),
                &self.local_path,
                &info.url,
                &item,
            )
            .await;
            let status = item.borrow().status;
            if status == Status::FatalError {
                return Err(item.borrow().error_string.clone());
            } else if status != Status::Success {
                log::warn!(target: "nextcloud.sync.propagator.cleanuppolls", "There was an error with file {} {}", info.file, item.borrow().error_string);
            } else {
                let fs_path = format!("{}{}", self.local_path, item.borrow().destination());
                let record = item
                    .borrow()
                    .to_sync_journal_file_record_with_inode(&fs_path);
                if self.journal.set_file_record(&record).is_err() {
                    log::warn!(target: "nextcloud.sync.propagator.cleanuppolls", "database error");
                    return Err("Error writing metadata to the database".to_owned());
                }
                self.journal
                    .set_upload_info(&info.file, &nc_journal::journal::UploadInfo::default());
            }
        }
        Ok(())
    }
}

/// `isFileTransferInstruction`.
fn is_file_transfer_instruction(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::Conflict | Instruction::New | Instruction::Sync | Instruction::TypeChange
    )
}

/// `shouldDiscoverLocally(path)` on a normalized, sorted path list.
pub fn should_discover_locally(style: LocalDiscoveryStyle, paths: &[String], path: &str) -> bool {
    if style == LocalDiscoveryStyle::FilesystemOnly {
        return true;
    }
    // The intention is that if "A/X" is in _localDiscoveryPaths:
    // - parent folders like "/", "A" will be discovered
    // - the folder itself "A/X" will be discovered
    // - subfolders like "A/X/Y" will be discovered
    let mut it = paths.partition_point(|p| crate::utility::qstring_cmp(p, path).is_lt());
    if it == paths.len() || !paths[it].starts_with(path) {
        // Maybe a subfolder of something in the list?
        if it != 0 && path.starts_with(paths[it - 1].as_str()) {
            let prev = &paths[it - 1];
            return prev.ends_with('/')
                || (path.len() > prev.len() && path.as_bytes()[prev.len()] <= b'/');
        }
        return false;
    }
    // maybe an exact match or an empty path?
    if paths[it].len() == path.len() || path.is_empty() {
        return true;
    }
    // Maybe a parent folder of something in the list?
    // check for a prefix + / match
    loop {
        if paths[it].len() > path.len() && paths[it].as_bytes()[path.len()] == b'/' {
            return true;
        }
        it += 1;
        if it == paths.len() || !paths[it].starts_with(path) {
            return false;
        }
    }
}

/// `checkErrorBlacklisting(item)`: true (and the item turned into an
/// ignored, blacklisted one) if it should not be synced because of the
/// error blacklist.
fn check_error_blacklisting(journal: &SyncJournalDb, item: &mut crate::item::SyncFileItem) -> bool {
    let entry = journal.error_blacklist_entry(&item.file);
    item.has_blacklist_entry = false;
    if !entry.is_valid() {
        return false;
    }
    item.has_blacklist_entry = true;
    // If duration has expired, it's not blacklisted anymore
    let now = crate::utility::current_secs_since_epoch();
    if now >= entry.last_try_time + entry.ignore_duration {
        log::info!(target: LOG, "blacklist entry for {} has expired!", item.file);
        return false;
    }
    // If the file has changed locally or on the server, the blacklist
    // entry no longer applies
    if item.direction == Direction::Up {
        // check the modtime
        if item.modtime == 0 || entry.last_try_modtime == 0 {
            return false;
        } else if item.modtime != entry.last_try_modtime {
            log::info!(target: LOG, "{} is blacklisted, but has changed mtime!", item.file);
            return false;
        } else if item.rename_target != entry.rename_target {
            log::info!(target: LOG, "{} is blacklisted, but rename target changed from {}", item.file, entry.rename_target);
            return false;
        }
    } else if item.direction == Direction::Down {
        // download, check the etag.
        if item.etag.is_empty() || entry.last_try_etag.is_empty() {
            log::info!(target: LOG, "{} one ETag is empty, no blacklisting", item.file);
            return false;
        } else if item.etag != entry.last_try_etag {
            log::info!(target: LOG, "{} is blacklisted, but has changed etag!", item.file);
            return false;
        }
    }
    let wait_seconds = entry.last_try_time + entry.ignore_duration - now;
    log::info!(target: LOG, "Item is on blacklist: {} retries: {} for another {wait_seconds} s", entry.file, entry.retry_count);
    // We need to indicate that we skip this file due to blacklisting
    // for reporting and for making sure we don't update the blacklist
    // entry yet.
    item.instruction = Instruction::Ignore;
    item.status = Status::BlacklistedError;
    let wait_seconds_str =
        crate::utility::duration_to_descriptive_string1((1000 * wait_seconds).max(0) as u64);
    item.error_string = format!(
        "{} (skipped due to earlier error, trying again in {})",
        entry.error_string, wait_seconds_str
    );
    true
}

/// `SyncEngine::slotItemDiscovered`.
fn slot_item_discovered(
    state: &Rc<RefCell<SyncState>>,
    cbs: &Rc<RefCell<EngineCallbacks>>,
    journal: &SyncJournalDb,
    local_path: &str,
    account: &Account,
    item: &SyncFileItemPtr,
) {
    if let Some(cb) = cbs.borrow_mut().item_discovered.as_mut() {
        cb(item);
    }
    {
        let file = item.borrow().file.clone();
        if nc_journal::utility::is_conflict_file(&file) {
            state.borrow_mut().seen_conflict_files.insert(file);
        }
    }
    let (instruction, is_dir, direction) = {
        let i = item.borrow();
        (i.instruction, i.is_directory(), i.direction)
    };
    if instruction == Instruction::UpdateMetadata && !is_dir {
        // For directories, metadata-only updates will be done after all their files are propagated.
        // Update the database now already:  New remote fileid or Etag or RemotePerm
        // Or for files that were detected as "resolved conflict".
        // Or a local inode/mtime change
        if direction == Direction::Down {
            let file = item.borrow().file.clone();
            let file_path = format!("{local_path}{file}");
            // If the 'W' remote permission changed, update the local filesystem
            let prev = journal
                .get_file_record(file.as_bytes())
                .ok()
                .flatten()
                .unwrap_or_default();
            {
                let i = item.borrow();
                if prev.is_valid()
                    && prev.remote_perm.has_permission(Permission::CanWrite)
                        != i.remote_perm.has_permission(Permission::CanWrite)
                {
                    let is_read_only = !i.remote_perm.is_null()
                        && !i.remote_perm.has_permission(Permission::CanWrite);
                    filesystem::set_file_read_only_weak(&file_path, is_read_only);
                }
                if i.is_permissions_invalid {
                    let is_read_only = !i.remote_perm.is_null()
                        && !i.remote_perm.has_permission(Permission::CanWrite);
                    filesystem::set_file_read_only(&file_path, is_read_only);
                }
            }
            let mut rec = item
                .borrow()
                .to_sync_journal_file_record_with_inode(&file_path);
            if rec.checksum_header.is_empty() {
                rec.checksum_header = prev.checksum_header.clone();
            }
            rec.server_has_ignored_files |= prev.server_has_ignored_files;
            // (virtual file metadata: the VFS is off)
            let modtime = item.borrow().modtime;
            if prev.modtime != modtime && !filesystem::set_mod_time(&file_path, modtime) {
                {
                    let mut i = item.borrow_mut();
                    i.instruction = Instruction::Error;
                    i.error_string = format!("Could not update file metadata: {file_path}");
                }
                if let Some(cb) = cbs.borrow_mut().item_completed.as_mut() {
                    cb(item, ErrorCategory::GenericError);
                }
                return;
            }
            // Updating the db happens on success
            if journal.set_file_record(&rec).is_err() {
                let mut i = item.borrow_mut();
                i.status = Status::NormalError;
                i.instruction = Instruction::Error;
                i.error_string =
                    format!("Could not set file record to local DB: {}", rec.path_str());
                log::warn!(target: LOG, "Could not set file record to local DB {}", rec.path_str());
            }
            // This might have changed the shared flag, so we must notify SyncFileStatusTracker for example
            if let Some(cb) = cbs.borrow_mut().item_completed.as_mut() {
                cb(item, ErrorCategory::NoError);
            }
        } else {
            // Update only outdated data from the disk.
            let i = item.borrow();
            let lock_info = SyncJournalFileLockInfo {
                locked: i.locked == crate::item::LockStatus::LockedItem,
                lock_time: i.lock_time,
                lock_timeout: i.lock_timeout,
                lock_owner_id: i.lock_owner_id.clone(),
                lock_owner_type: i.lock_owner_type as i64,
                lock_owner_display_name: i.lock_owner_display_name.clone(),
                // Upstream quirk kept: the editor app gets the display name.
                lock_editor_app: i.lock_owner_display_name.clone(),
                lock_token: i.lock_token.clone(),
            };
            if journal
                .update_local_metadata(&i.file, i.modtime, i.size, i.inode, &lock_info)
                .is_err()
            {
                log::warn!(target: LOG, "Could not update local metadata for file {}", i.file);
            }
        }
        state.borrow_mut().has_none_files = true;
        return;
    } else if instruction == Instruction::None {
        state.borrow_mut().has_none_files = true;
        let file = item.borrow().file.clone();
        if account.capabilities().upload_conflict_files()
            && nc_journal::utility::is_conflict_file(&file)
        {
            // For uploaded conflict files, files with no action performed on them should
            // be displayed: but we mustn't overwrite the instruction if something happens
            // to the file!
            let mut i = item.borrow_mut();
            i.error_string = "Unresolved conflict.".to_owned();
            i.instruction = Instruction::Ignore;
            i.status = Status::Conflict;
        }
        return;
    } else if instruction == Instruction::Remove && !item.borrow().is_selective_sync {
        state.borrow_mut().has_remove_file = true;
    } else if instruction == Instruction::Rename {
        // If a file (or every file) has been renamed, it means not al files where deleted
        state.borrow_mut().has_none_files = true;
    } else if (instruction == Instruction::TypeChange || instruction == Instruction::Sync)
        && direction == Direction::Up
    {
        // An upload of an existing file means that the file was left unchanged on the server
        // This counts as a NONE for detecting if all the files on the server were changed
        state.borrow_mut().has_none_files = true;
    }
    // check for blacklisting of this item.
    // if the item is on blacklist, the instruction was set to ERROR
    check_error_blacklisting(journal, &mut item.borrow_mut());
    let mut s = state.borrow_mut();
    s.needs_update = true;
    // Insert sorted
    let pos = {
        let i = item.borrow();
        s.sync_items
            .partition_point(|x| item_ordering(&x.borrow(), &i) == std::cmp::Ordering::Less)
    };
    s.sync_items.insert(pos, item.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_should_discover_locally() {
        // Mirrors TestLocalDiscovery::testLocalDiscoveryDecision expectations.
        let paths: Vec<String> = vec![
            "A/X".into(),
            "foo bar space/touch".into(),
            "foo/".into(),
            "zzz".into(),
            "zzzz".into(),
        ];
        let s = LocalDiscoveryStyle::DatabaseAndFilesystem;
        assert!(should_discover_locally(s, &paths, ""));
        assert!(should_discover_locally(s, &paths, "A"));
        assert!(should_discover_locally(s, &paths, "A/X"));
        assert!(should_discover_locally(s, &paths, "A/X/Y"));
        assert!(!should_discover_locally(s, &paths, "A/Y"));
        assert!(should_discover_locally(s, &paths, "foo bar space"));
        assert!(should_discover_locally(s, &paths, "foo"));
        assert!(should_discover_locally(s, &paths, "foo/bar"));
        assert!(!should_discover_locally(s, &paths, "B"));
        assert!(should_discover_locally(
            LocalDiscoveryStyle::FilesystemOnly,
            &[],
            "B"
        ));
    }
}
