// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: LGPL-2.1-or-later

//! `nc-journal`: the parts of the (unofficial) Rust port of the Nextcloud desktop
//! client that mirror upstream `src/common` and `src/csync`:
//!
//! * [`jhash`]: `c_jhash64`, used for the journal's `phash` primary key,
//! * [`journal`]: `SyncJournalDb` (the `.sync_xxxxxx.db` SQLite journal),
//! * [`exclude`]: `ExcludedFiles` (the csync exclude engine),
//! * [`checksums`]: checksum header helpers and calculators,
//! * [`remote_permissions`]: `RemotePermissions` (the `oc:permissions` string).
//!
//! Reference upstream: nextcloud/desktop tag `v34.0.5`.

pub mod checksums;
pub mod csync;
pub mod exclude;
pub mod jhash;
pub mod journal;
pub mod pinstate;
pub mod remote_permissions;
pub mod utility;
