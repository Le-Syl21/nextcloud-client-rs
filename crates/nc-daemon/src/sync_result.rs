// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/syncresult.{h,cpp}` and
// `Progress::isWarningKind` of `src/libsync/progressdispatcher.cpp`
// (nextcloud/desktop v34.0.5).

//! `SyncResult`: the outcome of a folder's last (or current) sync run.

use std::time::SystemTime;

use nc_sync::{Direction, Instruction, Status as ItemStatus, SyncFileItem};

/// `SyncResult::Status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncStatus {
    #[default]
    Undefined,
    NotYetStarted,
    SyncPrepare,
    SyncRunning,
    SyncAbortRequested,
    Success,
    Problem,
    Error,
    SetupError,
    Paused,
}

impl SyncStatus {
    /// `statusString()`.
    pub fn status_string(self) -> &'static str {
        match self {
            Self::Undefined => "Undefined",
            Self::NotYetStarted => "Not yet started",
            Self::SyncRunning => "Sync running",
            Self::Success => "Success",
            Self::Error => "Error",
            Self::SetupError => "Setup error",
            Self::SyncPrepare => "Preparing to sync",
            Self::Problem => "Success, some files were ignored.",
            Self::SyncAbortRequested => "Sync request cancelled",
            Self::Paused => "Sync paused",
        }
    }

    /// A stable machine-readable name (the `Q_ENUM` key), used by the
    /// control socket's JSON output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Undefined => "Undefined",
            Self::NotYetStarted => "NotYetStarted",
            Self::SyncPrepare => "SyncPrepare",
            Self::SyncRunning => "SyncRunning",
            Self::SyncAbortRequested => "SyncAbortRequested",
            Self::Success => "Success",
            Self::Problem => "Problem",
            Self::Error => "Error",
            Self::SetupError => "SetupError",
            Self::Paused => "Paused",
        }
    }
}

/// `Progress::isWarningKind(status)`.
pub fn is_warning_kind(kind: ItemStatus) -> bool {
    matches!(
        kind,
        ItemStatus::SoftError
            | ItemStatus::NormalError
            | ItemStatus::FatalError
            | ItemStatus::FileIgnored
            | ItemStatus::Conflict
            | ItemStatus::Restoration
            | ItemStatus::DetailError
            | ItemStatus::BlacklistedError
            | ItemStatus::FileLocked
            | ItemStatus::FileNameInvalid
            | ItemStatus::FileNameInvalidOnServer
            | ItemStatus::FileNameClash
    )
}

/// What the result remembers of an item (`SyncFileItemPtr` members such as
/// `_firstItemNew`): its path, rename target and error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemSummary {
    pub file: String,
    pub rename_target: String,
    pub destination: String,
    pub error_string: String,
}

impl ItemSummary {
    fn of(item: &SyncFileItem) -> Self {
        Self {
            file: item.file.clone(),
            rename_target: item.rename_target.clone(),
            destination: item.destination().to_owned(),
            error_string: item.error_string.clone(),
        }
    }
}

/// `SyncResult`.
#[derive(Clone, Debug, Default)]
pub struct SyncResult {
    status: SyncStatus,
    sync_time: Option<SystemTime>,
    folder: String,
    errors: Vec<String>,
    found_files_not_synced: bool,
    folder_structure_was_changed: bool,
    num_new_items: i32,
    num_removed_items: i32,
    num_updated_items: i32,
    num_renamed_items: i32,
    num_new_conflict_items: i32,
    num_old_conflict_items: i32,
    num_error_items: i32,
    num_locked_items: i32,
    first_item_new: Option<ItemSummary>,
    first_item_deleted: Option<ItemSummary>,
    first_item_updated: Option<ItemSummary>,
    first_item_renamed: Option<ItemSummary>,
    first_new_conflict_item: Option<ItemSummary>,
    first_item_error: Option<ItemSummary>,
    first_item_locked: Option<ItemSummary>,
}

impl SyncResult {
    pub fn new() -> Self {
        Self::default()
    }

    /// `reset()`.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn status(&self) -> SyncStatus {
        self.status
    }

    pub fn status_string(&self) -> &'static str {
        self.status.status_string()
    }

    /// `setStatus(stat)`: also records the time.
    pub fn set_status(&mut self, stat: SyncStatus) {
        self.status = stat;
        self.sync_time = Some(SystemTime::now());
    }

    pub fn sync_time(&self) -> Option<SystemTime> {
        self.sync_time
    }

    pub fn error_strings(&self) -> &[String] {
        &self.errors
    }

    pub fn append_error_string(&mut self, err: &str) {
        self.errors.push(err.to_owned());
    }

    /// `errorString()`: the first error.
    pub fn error_string(&self) -> String {
        self.errors.first().cloned().unwrap_or_default()
    }

    pub fn clear_errors(&mut self) {
        self.errors.clear();
    }

    pub fn set_folder(&mut self, folder: &str) {
        self.folder = folder.to_owned();
    }

    pub fn folder(&self) -> &str {
        &self.folder
    }

    pub fn found_files_not_synced(&self) -> bool {
        self.found_files_not_synced
    }

    pub fn folder_structure_was_changed(&self) -> bool {
        self.folder_structure_was_changed
    }

    pub fn num_new_items(&self) -> i32 {
        self.num_new_items
    }

    pub fn num_removed_items(&self) -> i32 {
        self.num_removed_items
    }

    pub fn num_updated_items(&self) -> i32 {
        self.num_updated_items
    }

    pub fn num_renamed_items(&self) -> i32 {
        self.num_renamed_items
    }

    pub fn num_new_conflict_items(&self) -> i32 {
        self.num_new_conflict_items
    }

    pub fn num_old_conflict_items(&self) -> i32 {
        self.num_old_conflict_items
    }

    pub fn set_num_old_conflict_items(&mut self, n: i32) {
        self.num_old_conflict_items = n;
    }

    pub fn num_error_items(&self) -> i32 {
        self.num_error_items
    }

    pub fn has_unresolved_conflicts(&self) -> bool {
        self.num_new_conflict_items + self.num_old_conflict_items > 0
    }

    pub fn num_locked_items(&self) -> i32 {
        self.num_locked_items
    }

    pub fn has_locked_files(&self) -> bool {
        self.num_locked_items > 0
    }

    pub fn first_item_new(&self) -> Option<&ItemSummary> {
        self.first_item_new.as_ref()
    }

    pub fn first_item_deleted(&self) -> Option<&ItemSummary> {
        self.first_item_deleted.as_ref()
    }

    pub fn first_item_updated(&self) -> Option<&ItemSummary> {
        self.first_item_updated.as_ref()
    }

    pub fn first_item_renamed(&self) -> Option<&ItemSummary> {
        self.first_item_renamed.as_ref()
    }

    pub fn first_new_conflict_item(&self) -> Option<&ItemSummary> {
        self.first_new_conflict_item.as_ref()
    }

    pub fn first_item_error(&self) -> Option<&ItemSummary> {
        self.first_item_error.as_ref()
    }

    pub fn first_item_locked(&self) -> Option<&ItemSummary> {
        self.first_item_locked.as_ref()
    }

    /// `processCompletedItem(item)`.
    pub fn process_completed_item(&mut self, item: &SyncFileItem) {
        if is_warning_kind(item.status) {
            // Count any error conditions, error strings will have priority anyway.
            self.found_files_not_synced = true;
        }
        if item.is_directory()
            && matches!(
                item.instruction,
                Instruction::New
                    | Instruction::TypeChange
                    | Instruction::Remove
                    | Instruction::Rename
            )
        {
            self.folder_structure_was_changed = true;
        }
        if item.status == ItemStatus::FileLocked {
            self.num_locked_items += 1;
            if self.first_item_locked.is_none() {
                self.first_item_locked = Some(ItemSummary::of(item));
            }
        }
        // Process the item to the gui
        if item.status == ItemStatus::FatalError || item.status == ItemStatus::NormalError {
            //: this displays an error string (%2) for a file %1
            self.append_error_string(&format!("{}: {}", item.file, item.error_string));
            self.num_error_items += 1;
            if self.first_item_error.is_none() {
                self.first_item_error = Some(ItemSummary::of(item));
            }
        } else if matches!(
            item.status,
            ItemStatus::Conflict
                | ItemStatus::FileNameInvalid
                | ItemStatus::FileNameInvalidOnServer
                | ItemStatus::FileNameClash
        ) {
            if item.instruction == Instruction::Conflict {
                self.num_new_conflict_items += 1;
                if self.first_new_conflict_item.is_none() {
                    self.first_new_conflict_item = Some(ItemSummary::of(item));
                }
            } else {
                self.num_old_conflict_items += 1;
            }
        } else if !item.has_error_status()
            && item.status != ItemStatus::FileIgnored
            && item.direction == Direction::Down
        {
            match item.instruction {
                Instruction::New | Instruction::TypeChange => {
                    self.num_new_items += 1;
                    if self.first_item_new.is_none() {
                        self.first_item_new = Some(ItemSummary::of(item));
                    }
                }
                Instruction::Remove => {
                    self.num_removed_items += 1;
                    if self.first_item_deleted.is_none() {
                        self.first_item_deleted = Some(ItemSummary::of(item));
                    }
                }
                Instruction::Sync => {
                    self.num_updated_items += 1;
                    if self.first_item_updated.is_none() {
                        self.first_item_updated = Some(ItemSummary::of(item));
                    }
                }
                Instruction::Rename => {
                    if self.first_item_renamed.is_none() {
                        self.first_item_renamed = Some(ItemSummary::of(item));
                    }
                    self.num_renamed_items += 1;
                }
                _ => {
                    // nothing.
                }
            }
        } else if item.instruction == Instruction::Ignore {
            self.found_files_not_synced = true;
        }
    }
}
