/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-FileCopyrightText: 2020 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! FakeFolder test harness: port of upstream `test/syncenginetestutils.{h,cpp}`
//! (nextcloud/desktop v34.0.5).
//!
//! Upstream sync tests build a [`FileInfo`] tree, create a `FakeFolder` that
//! writes it to a temporary directory (the local side) and serves it from an
//! in-memory WebDAV server (`FakeQNAM`, the remote side), then mutate either
//! side with a [`FileModifier`] and compare trees after `syncOnce()`.
//!
//! # Mapping to upstream
//!
//! | Upstream | Here | Status |
//! |---|---|---|
//! | `PathComponents` | [`PathComponents`] | ported |
//! | `FileModifier` | [`FileModifier`] trait | ported |
//! | `FileInfo` (tree, etag propagation, `A12_B12_C12_S12`, `==`, `toStringNoElide`, `printDbData`) | [`FileInfo`] | ported |
//! | `DiskFileModifier` | [`DiskFileModifier`] | ported |
//! | `FakeFolder::toDisk` / `fromDisk` | [`to_disk`] / [`from_disk`] | ported |
//! | `getFilePathFromUrl`, `sRootUrl*`, `sUploadUrl` | [`server::file_path_from_url`], [`server::ROOT_URL`]... | ported |
//! | `FakeQNAM` (`createRequest` routing, `errorPaths`, `setOverride`, server version) | [`FakeServer`] implementing [`nc_dav::Transport`] | ported |
//! | `FakePropfindReply` | `FakeServer` PROPFIND handler | ported |
//! | `FakeGetReply` | GET handler | ported |
//! | `FakeGetWithDataReply` | [`server::get_with_data_reply`] (for overrides) | ported |
//! | `FakePutReply` | PUT handler | ported |
//! | `FakeMkcolReply`, `FakeDeleteReply`, `FakeMoveReply` | handlers | ported |
//! | `FakeChunkMoveReply` (chunking v2 assembly MOVE) | MOVE handler on the uploads tree | ported (untested until the chunked uploader exists) |
//! | `FakeFileLockReply` (LOCK/UNLOCK) | handler | ported |
//! | `FakeErrorReply`, `FakeJsonErrorReply`, `FakePayloadReply`, `FakeJsonReply` | [`server::error_reply`], [`server::payload_reply`], [`server::json_reply`] | ported (no delays) |
//! | `FakeHangingReply` | [`FakeReply::Hang`] | ported |
//! | `DelayedReply<T>` | [`FakeReply::Delayed`] | ported |
//! | `FakePutMultiFileReply`, `forEachReplyPart` (bulk upload) | — | pending (Phase 3), answers 501 |
//! | `FakeCredentials` | — | pending (nc-dav credentials) |
//! | `FakeFolder` | [`FakeFolder`] | partial: trees, modifiers, server, error paths, override; no engine |
//! | `FakeFolder::syncOnce`, `scheduleSync`, `execUntil*`, `syncEngine()`, `account()`, `switchToVfs`, `enableEnforceWindowsFileNameCompatibility` | — | pending (needs `nc-sync`) |
//! | `FakeFolder::dbState`, `syncJournal()` | — | pending (needs the journal API) |
//! | `ItemCompletedSpy` | — | pending (needs `SyncFileItem`) |
//! | `findConflict` | [`find_conflict`] | ported |
//!
//! When `FakeFolder` gets its engine, it must also add the two manual
//! excludes upstream adds (`.~lock.*#` and `]*.~*`), set
//! `minimumFileAgeForUpload` to 0 and run an initial sync unless told not to.
//!
//! # Deliberate deviations
//!
//! Where upstream relies on a `Q_ASSERT` that would abort the test binary,
//! the Rust port either panics (programming error in the test) or, when the
//! situation can arise from a legitimate client request, answers with the
//! status a real server would send; each case is documented on the handler.

pub mod disk;
pub mod file_info;
pub mod folder;
pub mod path;
pub mod server;
mod util;

pub use disk::{DiskFileModifier, from_disk, to_disk};
pub use file_info::{
    EtagsAction, FileInfo, FileModifier, FolderQuota, LockChange, LockState, find_conflict,
    generate_etag, generate_file_id, print_db_data, to_string_no_elide,
};
pub use folder::{AbortTrigger, FakeFolder, ItemCompletedSpy, run_event_loop};
pub use path::PathComponents;
pub use server::{FakeReply, FakeServer, Override, ServerState};
pub use util::{block_on, from_secs, http_date};
