// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `nc-sync`: the sync engine of the (unofficial) Rust port of the Nextcloud
//! desktop client: discovery and reconciliation (`discovery`), propagation
//! (`propagator`) and the `SyncEngine` driving both (`engine`).
//!
//! Reference upstream: nextcloud/desktop tag `v34.0.5`, `src/libsync`.

pub mod discovery;
pub mod engine;
pub mod filesystem;
pub mod item;
pub mod local_discovery_tracker;
pub mod options;
pub mod progress;
pub mod propagator;
pub mod scheduled_sync;
pub mod touched_files;
pub mod utility;

pub use engine::{AnotherSyncNeeded, EngineAbortHandle, EngineCallbacks, SyncEngine};
pub use item::{
    Direction, ErrorCategory, Instruction, LockOwnerType, LockStatus, Status, SyncFileItem,
    SyncFileItemPtr, SyncFileItemVector,
};
pub use local_discovery_tracker::LocalDiscoveryTracker;
pub use options::SyncOptions;
pub use progress::{ProgressInfo, ProgressStatus};
