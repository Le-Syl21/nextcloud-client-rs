// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/progressdispatcher.{h,cpp}` (nextcloud/desktop
// v34.0.5), the parts the sync engine feeds: `ProgressInfo` with its status
// and its file and size counters.

//! `ProgressInfo`: the progress of a sync run, as given to the
//! `transmissionProgress` signal of the sync engine.
//!
//! Not ported: the estimates (`updateEstimates`, `totalProgress`,
//! `fileProgress`, the bandwidth/ETA smoothing driven by a 1 s timer) and the
//! `ProgressDispatcher` singleton, which are GUI features.

use std::collections::HashMap;

use nc_journal::csync::ItemType;

use crate::item::{Instruction, SyncFileItem};

/// `ProgressInfo::Status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProgressStatus {
    /// Emitted once at start
    #[default]
    Starting,
    /// Emitted once without _currentDiscoveredFolder when it starts,
    /// then for each folder.
    Discovery,
    /// Emitted once when reconcile starts
    Reconcile,
    /// Emitted during propagation, with progress data
    Propagation,
    /// Emitted once when done
    ///
    /// Except when SyncEngine jumps directly to finalize() without going
    /// through slotPropagationFinished().
    Done,
}

/// `ProgressInfo::Progress`: a completed/total pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    completed: i64,
    total: i64,
}

impl Progress {
    pub fn completed(&self) -> i64 {
        self.completed
    }

    pub fn remaining(&self) -> i64 {
        self.total - self.completed
    }

    /// `setCompleted`: bounded by the total.
    fn set_completed(&mut self, completed: i64) {
        self.completed = completed.min(self.total);
    }
}

/// The fields of a `SyncFileItem` the progress needs (upstream keeps a copy
/// of the whole item in `ProgressItem`).
#[derive(Clone, Debug)]
struct ProgressItem {
    is_size_dependent: bool,
    progress: Progress,
}

/// `ProgressInfo`.
#[derive(Clone, Debug, Default)]
pub struct ProgressInfo {
    pub status: ProgressStatus,
    /// `_currentItems`: the items being transferred, by file name.
    current_items: HashMap<String, ProgressItem>,
    pub current_discovered_remote_folder: String,
    pub current_discovered_local_folder: String,
    size_progress: Progress,
    file_progress: Progress,
    /// All size from completed jobs only.
    total_size_of_completed_jobs: i64,
    /// The last completed item's file name (`_lastCompletedItem`), empty
    /// when none.
    pub last_completed_item: String,
}

/// `shouldCountProgress`: skip any ignored, error or non-propagated files
/// and directories.
fn should_count_progress(item: &SyncFileItem) -> bool {
    !matches!(
        item.instruction,
        Instruction::None | Instruction::UpdateMetadata | Instruction::Ignore | Instruction::Error
    )
}

impl ProgressInfo {
    /// `isSizeDependent`: true if the size needs to be taken in account in
    /// the total amount of time.
    pub fn is_size_dependent(item: &SyncFileItem) -> bool {
        !item.is_directory()
            && matches!(
                item.instruction,
                Instruction::Conflict
                    | Instruction::Sync
                    | Instruction::New
                    | Instruction::TypeChange
            )
            && !matches!(
                item.item_type,
                ItemType::VirtualFile | ItemType::VirtualFileDehydration
            )
    }

    /// `reset()`: resets for a new sync run.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn status(&self) -> ProgressStatus {
        self.status
    }

    /// `adjustTotalsForFile`: called when a new item is discovered.
    pub fn adjust_totals_for_file(&mut self, item: &SyncFileItem) {
        if !should_count_progress(item) {
            return;
        }
        self.file_progress.total += i64::from(item.affected_items);
        if Self::is_size_dependent(item) {
            self.size_progress.total += item.size;
        }
    }

    pub fn total_files(&self) -> i64 {
        self.file_progress.total
    }

    pub fn completed_files(&self) -> i64 {
        self.file_progress.completed
    }

    pub fn total_size(&self) -> i64 {
        self.size_progress.total
    }

    pub fn completed_size(&self) -> i64 {
        self.size_progress.completed
    }

    /// `currentFile()`: number of a file that is currently in progress.
    pub fn current_file(&self) -> i64 {
        self.completed_files() + self.current_items.len() as i64
    }

    /// `setProgressComplete`.
    pub fn set_progress_complete(&mut self, item: &SyncFileItem) {
        if !should_count_progress(item) {
            return;
        }
        self.current_items.remove(&item.file);
        let completed = self.file_progress.completed + i64::from(item.affected_items);
        self.file_progress.set_completed(completed);
        if Self::is_size_dependent(item) {
            self.total_size_of_completed_jobs += item.size;
        }
        self.recompute_completed_size();
        self.last_completed_item = item.file.clone();
    }

    /// `setProgressItem`.
    pub fn set_progress_item(&mut self, item: &SyncFileItem, completed: i64) {
        if !should_count_progress(item) {
            return;
        }
        let entry = self
            .current_items
            .entry(item.file.clone())
            .or_insert(ProgressItem {
                is_size_dependent: false,
                progress: Progress::default(),
            });
        entry.is_size_dependent = Self::is_size_dependent(item);
        entry.progress.total = item.size;
        entry.progress.set_completed(completed);
        self.recompute_completed_size();
        // This seems dubious!
        self.last_completed_item.clear();
    }

    fn recompute_completed_size(&mut self) {
        let r = self.total_size_of_completed_jobs
            + self
                .current_items
                .values()
                .filter(|i| i.is_size_dependent)
                .map(|i| i.progress.completed)
                .sum::<i64>();
        self.size_progress.set_completed(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_size_counters() {
        let mut p = ProgressInfo::default();
        let item = SyncFileItem {
            file: "A/a".to_owned(),
            instruction: Instruction::New,
            size: 100,
            ..SyncFileItem::default()
        };
        p.adjust_totals_for_file(&item);
        assert_eq!(p.total_size(), 100);
        p.set_progress_item(&item, 40);
        assert_eq!(p.completed_size(), 40);
        p.set_progress_item(&item, 400);
        assert_eq!(p.completed_size(), 100);
        p.set_progress_complete(&item);
        assert_eq!(p.completed_size(), 100);
        assert_eq!(p.completed_files(), 1);
        let ignored = SyncFileItem {
            instruction: Instruction::Ignore,
            size: 7,
            ..SyncFileItem::default()
        };
        p.adjust_totals_for_file(&ignored);
        assert_eq!(p.total_size(), 100);
    }
}
