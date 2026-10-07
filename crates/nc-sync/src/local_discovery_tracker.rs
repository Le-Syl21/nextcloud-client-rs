// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/localdiscoverytracker.{h,cpp}` (nextcloud/desktop
// v34.0.5).

//! `LocalDiscoveryTracker`: tracks paths that need local discovery.
//!
//! Mostly a collection of touched local paths (from a file system watcher)
//! together with the paths of items that failed to sync, so the next sync
//! can rediscover them while reading everything else from the database.
//!
//! Upstream connects `slotItemCompleted` to `SyncEngine::itemCompleted` and
//! `slotSyncFinished` to `SyncEngine::finished`; here the owner calls them
//! from the engine callbacks.

use std::collections::BTreeSet;

use log::{debug, warn};

use crate::item::{Instruction, Status, SyncFileItem};

const LOG: &str = "nextcloud.sync.localdiscoverytracker";

/// `LocalDiscoveryTracker`.
#[derive(Debug, Default, Clone)]
pub struct LocalDiscoveryTracker {
    /// The paths that should be checked by the next local discovery.
    ///
    /// Mostly a collection of files the filewatchers have reported as touched.
    /// Also includes files that have had errors in the last sync run.
    local_discovery_paths: BTreeSet<String>,
    /// The paths that the current sync run used for local discovery.
    ///
    /// For failing syncs, this list will be merged into `local_discovery_paths`
    /// again when the sync is done to make sure everything is retried.
    previous_local_discovery_paths: BTreeSet<String>,
}

impl LocalDiscoveryTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `addTouchedPath(relativePath)`: adds a path that must be locally
    /// rediscovered later.
    pub fn add_touched_path(&mut self, relative_path: &str) {
        debug!(target: LOG, "inserted touched {relative_path}");
        self.local_discovery_paths.insert(relative_path.to_owned());
    }

    /// `startSyncFullDiscovery()`: call when a sync run starts that rediscovers all local files.
    pub fn start_sync_full_discovery(&mut self) {
        self.local_discovery_paths.clear();
        self.previous_local_discovery_paths.clear();
        debug!(target: LOG, "full discovery");
    }

    /// `startSyncPartialDiscovery()`: call when using a
    /// `DatabaseAndFilesystem` sync with the paths of `local_discovery_paths()`.
    pub fn start_sync_partial_discovery(&mut self) {
        debug!(
            target: LOG,
            "partial discovery with paths: {:?}", self.local_discovery_paths
        );
        self.previous_local_discovery_paths = std::mem::take(&mut self.local_discovery_paths);
    }

    /// `localDiscoveryPaths()`: access to the tracked paths.
    pub fn local_discovery_paths(&self) -> &BTreeSet<String> {
        &self.local_discovery_paths
    }

    /// `slotItemCompleted(item)`: success and failure of sync items adjust
    /// what the next sync is supposed to do.
    pub fn slot_item_completed(&mut self, item: &SyncFileItem) {
        // For successes, we want to wipe the file from the list to ensure we don't
        // rediscover it even if this overall sync fails.
        //
        // For failures, we want to add the file to the list so the next sync
        // will be able to retry it.
        if matches!(
            item.status,
            Status::Success | Status::FileIgnored | Status::Restoration | Status::Conflict
        ) || (item.status == Status::NoStatus
            && matches!(
                item.instruction,
                Instruction::None | Instruction::UpdateMetadata
            ))
        {
            if self.previous_local_discovery_paths.remove(&item.file) {
                debug!(target: LOG, "wiped successful item {}", item.file);
            }
            if !item.rename_target.is_empty()
                && self
                    .previous_local_discovery_paths
                    .remove(&item.rename_target)
            {
                debug!(target: LOG, "wiped successful item {}", item.rename_target);
            }
        } else {
            self.local_discovery_paths.insert(item.file.clone());
            warn!(target: LOG, "inserted error item {}", item.file);
        }
    }

    /// `slotSyncFinished(success)`: when a sync finishes,
    /// `previous_local_discovery_paths` must be reset.
    pub fn slot_sync_finished(&mut self, success: bool) {
        if success {
            debug!(
                target: LOG,
                "sync success, forgetting last sync's local discovery path list"
            );
        } else {
            // On overall-failure we can't forget about last sync's local discovery
            // paths yet, reuse them for the next sync again.
            let previous = std::mem::take(&mut self.previous_local_discovery_paths);
            self.local_discovery_paths.extend(previous);
            warn!(
                target: LOG,
                "sync failed, keeping last sync's local discovery path list"
            );
        }
        self.previous_local_discovery_paths.clear();
    }
}
