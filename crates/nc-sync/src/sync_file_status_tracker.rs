// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/syncfilestatustracker.{h,cpp}`
// (nextcloud/desktop v34.0.5).

//! `SyncFileStatusTracker`: takes care of tracking the status of individual
//! files as they go through the sync engine, to be reported as overlay icons
//! in the shell (or by `ncsync status`).
//!
//! The sync engine owns one ([`crate::SyncEngine::sync_file_status_tracker`])
//! and calls its slots where upstream connects them to the engine's signals
//! (`aboutToPropagate`, `itemCompleted`, `started`, `finished`, the
//! discovery's `silentlyExcluded`), before the engine's own callbacks, like
//! the connections made in the tracker's constructor come first upstream.
//! `fileStatusChanged` goes to the receivers added with
//! [`SyncFileStatusTracker::connect_file_status_changed`]; they must not
//! call back into the tracker.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use nc_journal::exclude::ExcludedFiles;
use nc_journal::journal::SyncJournalDb;
use nc_journal::remote_permissions::Permission;

use crate::item::{Instruction, Status, SyncFileItem, SyncFileItemPtr, SyncFileItemVector};
use crate::sync_file_status::{SyncFileStatus, SyncFileStatusTag};

const LOG: &str = "nextcloud.sync.statustracker";

/// Paths are compared case-insensitively on Windows and macOS (a compile
/// time choice upstream: it should match `Utility::fsCasePreserving`
/// without paying for the runtime check on every comparison).
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// `pathCompare`: `QString::compare` (UTF-16 code unit order).
fn path_compare(lhs: &str, rhs: &str) -> Ordering {
    if CASE_INSENSITIVE {
        crate::utility::qstring_cmp(&lhs.to_lowercase(), &rhs.to_lowercase())
    } else {
        crate::utility::qstring_cmp(lhs, rhs)
    }
}

/// `pathStartsWith`.
fn path_starts_with(lhs: &str, rhs: &str) -> bool {
    if CASE_INSENSITIVE {
        lhs.to_lowercase().starts_with(&rhs.to_lowercase())
    } else {
        lhs.starts_with(rhs)
    }
}

/// A key of `ProblemsMap`, ordered by `PathComparator`.
#[derive(Clone, Debug)]
struct PathKey(String);

impl PartialEq for PathKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for PathKey {}
impl PartialOrd for PathKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PathKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // This will make sure that the map is ordered and queried case-insensitively on macOS and Windows.
        path_compare(&self.0, &other.0)
    }
}

type ProblemsMap = BTreeMap<PathKey, SyncFileStatusTag>;

/// `SharedFlag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharedFlag {
    UnknownShared,
    NotShared,
    Shared,
}

/// `PathKnownFlag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKnownFlag {
    PathUnknown,
    PathKnown,
}

/// Whether this item should get an ERROR icon through the Socket API.
///
/// The Socket API should only present serious, permanent errors to the user.
/// In particular SoftErrors should just retain their 'needs to be synced'
/// icon as the problem is most likely going to resolve itself quickly and
/// automatically.
fn has_error_status(item: &SyncFileItem) -> bool {
    let status = item.status;
    item.instruction == Instruction::Error
        || status == Status::NormalError
        || status == Status::FatalError
        || status == Status::DetailError
        || status == Status::BlacklistedError
        || item.has_blacklist_entry
}

fn has_excluded_status(item: &SyncFileItem) -> bool {
    let status = item.status;
    item.instruction == Instruction::Ignore
        || status == Status::FileIgnored
        || status == Status::Conflict
        || status == Status::Restoration
        || status == Status::FileLocked
}

fn shared_flag(item: &SyncFileItem) -> SharedFlag {
    if item.remote_perm.has_permission(Permission::IsShared) {
        SharedFlag::Shared
    } else {
        SharedFlag::NotShared
    }
}

/// Instructions that result in propagation (and mark the path as syncing).
fn is_propagating(instruction: Instruction) -> bool {
    !matches!(
        instruction,
        Instruction::None | Instruction::UpdateMetadata | Instruction::Ignore | Instruction::Error
    )
}

/// Receiver of `fileStatusChanged(systemFileName, fileStatus)`.
pub type FileStatusChangedCallback = Box<dyn FnMut(&str, SyncFileStatus)>;

/// `SyncFileStatusTracker`.
pub struct SyncFileStatusTracker {
    /// `_syncEngine->localPath()`: ends with '/'.
    local_path: String,
    journal: Arc<SyncJournalDb>,
    excluded_files: Rc<RefCell<ExcludedFiles>>,
    /// `_syncEngine->ignoreHiddenFiles()` (kept up to date by the engine).
    ignore_hidden_files: bool,
    sync_problems: ProblemsMap,
    sync_silent_excludes: ProblemsMap,
    dirty_paths: HashSet<String>,
    /// Counts the number direct children currently being synced (has unfinished propagation jobs).
    /// We'll show a file/directory as SYNC as long as its sync count is > 0.
    /// A directory that starts/ends propagation will in turn increase/decrease its own parent by 1.
    sync_count: HashMap<String, i32>,
    file_status_changed: Vec<FileStatusChangedCallback>,
}

impl SyncFileStatusTracker {
    /// `SyncFileStatusTracker(syncEngine)`: what it needs of the engine.
    pub fn new(
        local_path: &str,
        journal: Arc<SyncJournalDb>,
        excluded_files: Rc<RefCell<ExcludedFiles>>,
    ) -> Self {
        Self {
            local_path: local_path.to_owned(),
            journal,
            excluded_files,
            ignore_hidden_files: false,
            sync_problems: ProblemsMap::new(),
            sync_silent_excludes: ProblemsMap::new(),
            dirty_paths: HashSet::new(),
            sync_count: HashMap::new(),
            file_status_changed: Vec::new(),
        }
    }

    /// Connects a receiver of `fileStatusChanged(systemFileName, fileStatus)`.
    pub fn connect_file_status_changed(&mut self, cb: FileStatusChangedCallback) {
        self.file_status_changed.push(cb);
    }

    pub(crate) fn set_ignore_hidden_files(&mut self, ignore: bool) {
        self.ignore_hidden_files = ignore;
    }

    fn emit_file_status_changed(&mut self, system_file_name: &str, status: SyncFileStatus) {
        for cb in &mut self.file_status_changed {
            cb(system_file_name, status);
        }
    }

    /// `lookupProblem`.
    fn lookup_problem(path_to_match: &str, problem_map: &ProblemsMap) -> SyncFileStatusTag {
        for (problem_path, &severity) in problem_map.range(PathKey(path_to_match.to_owned())..) {
            let problem_path = &problem_path.0;
            if path_compare(problem_path, path_to_match) == Ordering::Equal {
                return severity;
            } else if severity == SyncFileStatusTag::StatusError
                && path_starts_with(problem_path, path_to_match)
                && (path_to_match.is_empty()
                    || problem_path
                        .encode_utf16()
                        .nth(path_to_match.encode_utf16().count())
                        == Some(u16::from(b'/')))
            {
                return SyncFileStatusTag::StatusWarning;
            } else if !path_starts_with(problem_path, path_to_match) {
                // Starting at lower_bound we get the first path that is not smaller,
                // since: "a/" < "a/aa" < "a/aa/aaa" < "a/ab/aba"
                // If problemMap keys are ["a/aa/aaa", "a/ab/aba"] and pathToMatch == "a/aa",
                // lower_bound(pathToMatch) will point to "a/aa/aaa", and the moment that
                // problemPath.startsWith(pathToMatch) == false, we know that we've looked
                // at everything that interest us.
                break;
            }
        }
        SyncFileStatusTag::StatusNone
    }

    /// `fileStatus(relativePath)`.
    pub fn file_status(&self, relative_path: &str) -> SyncFileStatus {
        debug_assert!(!relative_path.ends_with('/'));
        if relative_path.is_empty() {
            // This is the root sync folder, it doesn't have an entry in the database and won't be walked by csync, so resolve manually.
            return self.resolve_sync_and_error_status(
                "",
                SharedFlag::NotShared,
                PathKnownFlag::PathKnown,
            );
        }
        // The SyncEngine won't notify us at all for CSYNC_FILE_SILENTLY_EXCLUDED
        // and CSYNC_FILE_EXCLUDE_AND_REMOVE excludes. Even though it's possible
        // that the status of CSYNC_FILE_EXCLUDE_LIST excludes will change if the user
        // update the exclude list at runtime and doing it statically here removes
        // our ability to notify changes through the fileStatusChanged signal,
        // it's an acceptable compromise to treat all exclude types the same.
        // Update: This extra check shouldn't hurt even though silently excluded files
        // are now available via slotAddSilentlyExcluded().
        if self.excluded_files.borrow().is_excluded(
            &format!("{}{relative_path}", self.local_path),
            &self.local_path,
            self.ignore_hidden_files,
        ) {
            return SyncFileStatusTag::StatusExcluded.into();
        }
        if self.dirty_paths.contains(relative_path) {
            return SyncFileStatusTag::StatusSync.into();
        }
        // First look it up in the database to know if it's shared
        if let Ok(Some(rec)) = self.journal.get_file_record(relative_path.as_bytes())
            && rec.is_valid()
        {
            let flag = if rec.remote_perm.has_permission(Permission::IsShared) {
                SharedFlag::Shared
            } else {
                SharedFlag::NotShared
            };
            return self.resolve_sync_and_error_status(
                relative_path,
                flag,
                PathKnownFlag::PathKnown,
            );
        }
        // Must be a new file not yet in the database, check if it's syncing or has an error.
        self.resolve_sync_and_error_status(
            relative_path,
            SharedFlag::NotShared,
            PathKnownFlag::PathUnknown,
        )
    }

    /// `slotPathTouched(fileName)`: an absolute path inside the folder.
    pub fn slot_path_touched(&mut self, file_name: &str) {
        let folder_path = self.local_path.clone();
        debug_assert!(file_name.starts_with(&folder_path));
        let local_path = file_name
            .get(folder_path.len()..)
            .unwrap_or_default()
            .to_owned();
        self.dirty_paths.insert(local_path);
        self.emit_file_status_changed(file_name, SyncFileStatusTag::StatusSync.into());
    }

    /// `slotAddSilentlyExcluded(folderPath)`: path relative to folder.
    pub fn slot_add_silently_excluded(&mut self, folder_path: &str) {
        self.sync_problems.insert(
            PathKey(folder_path.to_owned()),
            SyncFileStatusTag::StatusExcluded,
        );
        self.sync_silent_excludes.insert(
            PathKey(folder_path.to_owned()),
            SyncFileStatusTag::StatusExcluded,
        );
        let status = self.resolve_sync_and_error_status(
            folder_path,
            SharedFlag::NotShared,
            PathKnownFlag::PathKnown,
        );
        let dest = self.get_system_destination(folder_path);
        self.emit_file_status_changed(&dest, status);
    }

    /// `slotCheckAndRemoveSilentlyExcluded(folderPath)`.
    pub fn slot_check_and_remove_silently_excluded(&mut self, folder_path: &str) {
        if self
            .sync_silent_excludes
            .remove(&PathKey(folder_path.to_owned()))
            .is_some()
        {
            let dest = self.get_system_destination(folder_path);
            self.emit_file_status_changed(&dest, SyncFileStatusTag::StatusUpToDate.into());
        }
    }

    /// The parent of a path (`left(lastIndexOf('/'))`), the root for a
    /// top-level path, `None` for the root.
    fn parent_of(relative_path: &str) -> Option<String> {
        debug_assert!(!relative_path.ends_with('/'));
        match relative_path.rfind('/') {
            Some(i) => Some(relative_path[..i].to_owned()),
            None if !relative_path.is_empty() => Some(String::new()),
            None => None,
        }
    }

    fn status_for(&self, relative_path: &str, shared_flag: SharedFlag) -> SyncFileStatus {
        if shared_flag == SharedFlag::UnknownShared {
            self.file_status(relative_path)
        } else {
            self.resolve_sync_and_error_status(relative_path, shared_flag, PathKnownFlag::PathKnown)
        }
    }

    /// `incSyncCountAndEmitStatusChanged`.
    fn inc_sync_count_and_emit_status_changed(
        &mut self,
        relative_path: &str,
        shared_flag: SharedFlag,
    ) {
        // Will return 0 (and increase to 1) if the path wasn't in the map yet
        let entry = self.sync_count.entry(relative_path.to_owned()).or_insert(0);
        let count = *entry;
        *entry += 1;
        if count == 0 {
            let status = self.status_for(relative_path, shared_flag);
            let dest = self.get_system_destination(relative_path);
            self.emit_file_status_changed(&dest, status);
            // We passed from OK to SYNC, increment the parent to keep it marked as
            // SYNC while we propagate ourselves and our own children.
            if let Some(parent) = Self::parent_of(relative_path) {
                self.inc_sync_count_and_emit_status_changed(&parent, SharedFlag::UnknownShared);
            }
        }
    }

    /// `decSyncCountAndEmitStatusChanged`.
    fn dec_sync_count_and_emit_status_changed(
        &mut self,
        relative_path: &str,
        shared_flag: SharedFlag,
    ) {
        let entry = self.sync_count.entry(relative_path.to_owned()).or_insert(0);
        *entry -= 1;
        let count = *entry;
        if count == 0 {
            // Remove from the map, same as 0
            self.sync_count.remove(relative_path);
            let status = self.status_for(relative_path, shared_flag);
            let dest = self.get_system_destination(relative_path);
            self.emit_file_status_changed(&dest, status);
            // We passed from SYNC to OK, decrement our parent.
            if let Some(parent) = Self::parent_of(relative_path) {
                self.dec_sync_count_and_emit_status_changed(&parent, SharedFlag::UnknownShared);
            }
        }
    }

    /// `slotAboutToPropagate(items)`.
    pub fn slot_about_to_propagate(&mut self, items: &SyncFileItemVector) {
        debug_assert!(self.sync_count.is_empty());
        let old_problems = std::mem::take(&mut self.sync_problems);
        for item in items {
            let (destination, error, excluded, flag, instruction) = {
                let i = item.borrow();
                if i.instruction == Instruction::Rename {
                    log::info!(target: LOG, "Investigating {} {:?} {:?} {:?} {:?} {} {} {}", i.destination(), i.status, i.instruction, i.direction, i.item_type, i.file, i.original_file, i.rename_target);
                } else {
                    log::info!(target: LOG, "Investigating {} {:?} {:?} {:?} {:?}", i.destination(), i.status, i.instruction, i.direction, i.item_type);
                }
                (
                    i.destination().to_owned(),
                    has_error_status(&i),
                    has_excluded_status(&i),
                    shared_flag(&i),
                    i.instruction,
                )
            };
            self.dirty_paths.remove(&destination);
            if error {
                self.sync_problems
                    .insert(PathKey(destination.clone()), SyncFileStatusTag::StatusError);
                self.sync_silent_excludes
                    .remove(&PathKey(destination.clone()));
                self.invalidate_parent_paths(&destination);
            } else if excluded {
                self.sync_problems.insert(
                    PathKey(destination.clone()),
                    SyncFileStatusTag::StatusExcluded,
                );
                self.sync_silent_excludes
                    .remove(&PathKey(destination.clone()));
            }
            if instruction != Instruction::Remove {
                item.borrow_mut().discovery_result.clear();
            }
            if is_propagating(instruction) {
                // Mark this path as syncing for instructions that will result in propagation.
                self.inc_sync_count_and_emit_status_changed(&destination, flag);
            } else {
                let status = self.resolve_sync_and_error_status(
                    &destination,
                    flag,
                    PathKnownFlag::PathKnown,
                );
                let dest = self.get_system_destination(&destination);
                self.emit_file_status_changed(&dest, status);
            }
        }
        // Some metadata status won't trigger files to be synced, make sure that we
        // push the OK status for dirty files that don't need to be propagated.
        // Swap into a copy since fileStatus() reads _dirtyPaths to determine the status
        let old_dirty_paths = std::mem::take(&mut self.dirty_paths);
        for old_dirty_path in &old_dirty_paths {
            let status = self.file_status(old_dirty_path);
            let dest = self.get_system_destination(old_dirty_path);
            self.emit_file_status_changed(&dest, status);
        }
        // Make sure to push any status that might have been resolved indirectly since the last sync
        // (like an error file being deleted from disk)
        let mut old_problems = old_problems;
        for path in self.sync_problems.keys() {
            old_problems.remove(path);
        }
        for (path, severity) in old_problems {
            if severity == SyncFileStatusTag::StatusError {
                self.invalidate_parent_paths(&path.0);
            }
            let status = self.file_status(&path.0);
            let dest = self.get_system_destination(&path.0);
            self.emit_file_status_changed(&dest, status);
        }
    }

    /// `slotItemCompleted(item)`.
    pub fn slot_item_completed(&mut self, item: &SyncFileItemPtr) {
        let (destination, error, excluded, flag, instruction) = {
            let i = item.borrow();
            log::debug!(target: LOG, "Item completed {} {:?} {:?}", i.destination(), i.status, i.instruction);
            (
                i.destination().to_owned(),
                has_error_status(&i),
                has_excluded_status(&i),
                shared_flag(&i),
                i.instruction,
            )
        };
        if error {
            self.sync_problems
                .insert(PathKey(destination.clone()), SyncFileStatusTag::StatusError);
            self.invalidate_parent_paths(&destination);
        } else if excluded {
            self.sync_problems.insert(
                PathKey(destination.clone()),
                SyncFileStatusTag::StatusExcluded,
            );
        } else {
            self.sync_problems.remove(&PathKey(destination.clone()));
        }
        self.sync_silent_excludes
            .remove(&PathKey(destination.clone()));
        if is_propagating(instruction) {
            // decSyncCount calls *must* be symmetric with incSyncCount calls in slotAboutToPropagate
            self.dec_sync_count_and_emit_status_changed(&destination, flag);
        } else {
            let status =
                self.resolve_sync_and_error_status(&destination, flag, PathKnownFlag::PathKnown);
            let dest = self.get_system_destination(&destination);
            self.emit_file_status_changed(&dest, status);
        }
    }

    /// `slotSyncFinished`.
    pub fn slot_sync_finished(&mut self) {
        // Clear the sync counts to reduce the impact of unsymetrical inc/dec calls (e.g. when directory job abort)
        let old_sync_count = std::mem::take(&mut self.sync_count);
        for key in old_sync_count.keys() {
            // Don't announce folders, fileStatus expect only paths without '/', otherwise it asserts
            if key.ends_with('/') {
                continue;
            }
            let status = self.file_status(key);
            let dest = self.get_system_destination(key);
            self.emit_file_status_changed(&dest, status);
        }
    }

    /// `slotSyncEngineRunningChanged`.
    pub fn slot_sync_engine_running_changed(&mut self) {
        let status =
            self.resolve_sync_and_error_status("", SharedFlag::NotShared, PathKnownFlag::PathKnown);
        let dest = self.get_system_destination("");
        self.emit_file_status_changed(&dest, status);
    }

    /// `resolveSyncAndErrorStatus`.
    fn resolve_sync_and_error_status(
        &self,
        relative_path: &str,
        shared_flag: SharedFlag,
        is_path_known: PathKnownFlag,
    ) -> SyncFileStatus {
        // If it's a new file and that we're not syncing it yet,
        // don't show any icon and wait for the filesystem watcher to trigger a sync.
        let mut status = SyncFileStatus::new(if is_path_known == PathKnownFlag::PathKnown {
            SyncFileStatusTag::StatusUpToDate
        } else {
            SyncFileStatusTag::StatusNone
        });
        if self.sync_count.get(relative_path).copied().unwrap_or(0) != 0 {
            status.set(SyncFileStatusTag::StatusSync);
        } else {
            // After a sync finished, we need to show the users issues from that last sync like the activity list does.
            // Also used for parent directories showing a warning for an error child.
            let problem_status = Self::lookup_problem(relative_path, &self.sync_problems);
            if problem_status != SyncFileStatusTag::StatusNone {
                status.set(problem_status);
            }
        }
        debug_assert!(
            shared_flag != SharedFlag::UnknownShared,
            "The shared status needs to have been fetched from a SyncFileItem or the DB at this point."
        );
        if shared_flag == SharedFlag::Shared {
            status.set_shared(true);
        }
        status
    }

    /// `invalidateParentPaths`.
    fn invalidate_parent_paths(&mut self, path: &str) {
        let split_path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for i in 0..split_path.len() {
            let parent_path = split_path[..i].join("/");
            let status = self.file_status(&parent_path);
            let dest = self.get_system_destination(&parent_path);
            self.emit_file_status_changed(&dest, status);
        }
    }

    /// `getSystemDestination`.
    fn get_system_destination(&self, relative_path: &str) -> String {
        let mut system_path = format!("{}{relative_path}", self.local_path);
        // SyncEngine::localPath() has a trailing slash, make sure to remove it if the
        // destination is empty.
        if system_path.ends_with('/') {
            system_path.pop();
        }
        system_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_lookup_problem() {
        let mut map = ProblemsMap::new();
        map.insert(PathKey("A/a1".into()), SyncFileStatusTag::StatusError);
        map.insert(PathKey("B".into()), SyncFileStatusTag::StatusExcluded);
        let l = SyncFileStatusTracker::lookup_problem;
        assert_eq!(l("", &map), SyncFileStatusTag::StatusWarning);
        assert_eq!(l("A", &map), SyncFileStatusTag::StatusWarning);
        assert_eq!(l("A/a1", &map), SyncFileStatusTag::StatusError);
        assert_eq!(l("A/a", &map), SyncFileStatusTag::StatusNone);
        assert_eq!(l("B", &map), SyncFileStatusTag::StatusExcluded);
        assert_eq!(l("C", &map), SyncFileStatusTag::StatusNone);
    }
}
