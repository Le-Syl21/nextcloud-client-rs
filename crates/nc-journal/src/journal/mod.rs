// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Port of upstream `src/common/syncjournaldb.{h,cpp}` and the parts of
// `src/common/ownsql.cpp` it relies on (nextcloud/desktop v34.0.5).

//! The sync journal (`SyncJournalDb`): the `.sync_xxxxxx.db` SQLite database
//! the official client keeps at the root of every sync folder.
//!
//! The on-disk format is kept byte-compatible with upstream so that a folder
//! can be handed over between the official client and this port: same tables,
//! same columns, same encodings, same `phash` (`c_jhash64`), same
//! `parent_hash()` SQL function used by the `metadata_parent` index.
//!
//! Faithfulness notes (things that look odd but are upstream behaviour):
//!
//! * upstream's `SqlQuery::exec()` does not step `PRAGMA`/`SELECT` statements;
//!   only `next()` does. Hence `PRAGMA synchronous`, `PRAGMA case_sensitive_like`,
//!   `PRAGMA temp_store` and `PRAGMA wal_checkpoint(FULL)` are prepared but
//!   never run by upstream. We do the same (see [`Inner::check_connect`]).
//! * every public method locks; callbacks of the `get_*`/`list_*` row
//!   iterators are invoked after the lock is released (upstream uses a
//!   recursive mutex and calls them while iterating) so they may call back
//!   into the journal.

mod record;
#[cfg(test)]
mod tests;

pub use record::{
    ConflictRecord, ErrorCategory, FolderQuota, SyncJournalErrorBlacklistRecord,
    SyncJournalFileLockInfo, SyncJournalFileRecord,
};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use log::{debug, error, info, warn};
use md5::{Digest, Md5};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::{ToSqlOutput, Value, ValueRef};
use rusqlite::{Connection, OpenFlags, Row, Statement, ToSql, ffi};

use crate::checksums::parse_checksum_header;
use crate::csync::{ItemType, JournalDbEncryptionStatus};
use crate::jhash::c_jhash64;
use crate::pinstate::PinState;
use crate::remote_permissions::RemotePermissions;
use crate::utility;

const LOG_TARGET: &str = "nextcloud.sync.database";

/// Client version written to (and compared with) the `version` table.
///
/// Upstream writes `MIRALL_VERSION_{MAJOR,MINOR,PATCH,BUILD}`. We write the
/// version of the upstream release whose behaviour we implement, so the
/// official client sees a journal "written by itself" on hand-over.
pub const CLIENT_VERSION: (i64, i64, i64, u64) = (
    crate::UPSTREAM_VERSION.0 as i64,
    crate::UPSTREAM_VERSION.1 as i64,
    crate::UPSTREAM_VERSION.2 as i64,
    0,
);

const MAJOR_VERSION_3: i64 = 3;
const MINOR_VERSION_16: i64 = 16;

/// SQL expression to check whether `path.startswith(prefix + '/')`.
/// Note: `'/' + 1 == '0'`.
macro_rules! is_prefix_path_of {
    ($prefix:literal, $path:literal) => {
        concat!(
            "(",
            $path,
            " > (",
            $prefix,
            "||'/') AND ",
            $path,
            " < (",
            $prefix,
            "||'0'))"
        )
    };
}

macro_rules! is_prefix_path_or_equal {
    ($prefix:literal, $path:literal) => {
        concat!(
            "(",
            $path,
            " == ",
            $prefix,
            " OR ",
            is_prefix_path_of!($prefix, $path),
            ")"
        )
    };
}

macro_rules! get_file_record_query {
    () => {
        concat!(
            "SELECT path, inode, modtime, type, md5, fileid, remotePerm, filesize,",
            "  ignoredChildrenRemote, contentchecksumtype.name || ':' || contentChecksum, e2eMangledName, isE2eEncrypted, e2eCertificateFingerprint, ",
            "  lock, lockOwnerDisplayName, lockOwnerId, lockType, lockOwnerEditor, lockTime, lockTimeout, lockToken, isShared, lastShareStateFetchedTimestmap, ",
            "  sharedByMe, isLivePhoto, livePhotoFile, quotaBytesUsed, quotaBytesAvailable",
            " FROM metadata",
            "  LEFT JOIN checksumtype as contentchecksumtype ON metadata.contentChecksumTypeId == contentchecksumtype.id"
        )
    };
}

/// Errors reported by the journal.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// `checkConnect()` failed (could not open the database, file vanished,
    /// simulated failure...).
    #[error("Failed to connect database.")]
    Connect,
    /// An SQLite error.
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
}

pub type Result<T, E = JournalError> = std::result::Result<T, E>;

/// `SyncJournalDb::DownloadInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadInfo {
    pub tmpfile: String,
    pub etag: Vec<u8>,
    pub error_count: i32,
    pub valid: bool,
}

/// `SyncJournalDb::UploadInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UploadInfo {
    pub chunk_upload_v1: i32,
    pub transferid: u32,
    pub size: i64,
    pub modtime: i64,
    pub error_count: i32,
    pub valid: bool,
    pub content_checksum: Vec<u8>,
}

impl UploadInfo {
    /// Whether this entry refers to a chunked upload that can be continued
    /// (as opposed to a small file transfer stored so we can detect the case
    /// where the upload succeeded but the connection dropped before the answer).
    pub fn is_chunked(&self) -> bool {
        self.transferid != 0
    }
}

/// `SyncJournalDb::PollInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollInfo {
    /// The relative path of a file.
    pub file: String,
    /// The poll url (this poll info is invalid if the url is empty).
    pub url: String,
    /// The modtime of the file being uploaded.
    pub modtime: i64,
    pub file_size: i64,
}

/// `SyncJournalDb::SelectiveSyncListType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum SelectiveSyncListType {
    /// Folders unselected in the selective sync dialog.
    BlackList = 1,
    /// Big shared folders that are nevertheless synced.
    WhiteList = 2,
    /// Big folders not yet confirmed by the user.
    UndecidedList = 3,
    /// Encrypted folders to remove from the blacklist when E2EE gets set up.
    E2eFoldersToRemoveFromBlacklist = 4,
}

/// Return value of [`SyncJournalDb::has_hydrated_or_dehydrated_files`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HasHydratedDehydrated {
    pub has_hydrated: bool,
    pub has_dehydrated: bool,
}

/// `SyncJournalDb::getPHash`: the `phash` of a relative path.
///
/// On macOS upstream first normalizes the path to NFC; not implemented yet
/// (Linux is the target of this port).
pub fn get_phash(file: &[u8]) -> i64 {
    c_jhash64(file, 0) as i64
}

/// `SyncJournalDb::makeDbName`: the journal file name for a folder,
/// `.sync_<first 6 bytes of md5("user@url:remotePath") in hex>.db`.
///
/// `remote_url` must be rendered exactly like upstream's `QUrl::toString()`
/// of the account URL (the GUI passes `account->url()`, `nextcloudcmd` passes
/// the credential-free URL). The function also checks that the file can be
/// created in `local_path`, like upstream, but always returns the name.
pub fn make_db_name(local_path: &Path, remote_url: &str, remote_path: &str, user: &str) -> String {
    let key = format!("{user}@{remote_url}:{remote_path}");
    let digest = Md5::digest(key.as_bytes());
    let hex: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    let journal_path = format!(".sync_{hex}.db");

    let file = local_path.join(&journal_path);
    // If it exists already, the path is clearly usable
    if file.exists() {
        return journal_path;
    }
    // Try to create a file there
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&file)
    {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&file);
        }
        Err(e) => {
            // Error during creation, just keep the original and throw errors later
            warn!(target: LOG_TARGET, "Could not find a writable database path {} {e}", file.display());
        }
    }
    journal_path
}

/// `SyncJournalDb::maybeMigrateDb`: migrate a legacy `.csync_journal.db` to
/// the new path, if necessary. Returns `false` on error.
///
/// `local_path` is concatenated as a string, like upstream (it is expected to
/// end with a `/`). Like upstream, a missing `-wal` or `-shm` file of the old
/// journal makes the function return `false` after the main file was moved.
pub fn maybe_migrate_db(local_path: &str, absolute_journal_path: &str) -> bool {
    let old_db_name = format!("{local_path}.csync_journal.db");
    if !Path::new(&old_db_name).exists() {
        return true;
    }
    let old_shm = format!("{old_db_name}-shm");
    let old_wal = format!("{old_db_name}-wal");
    let new_db_name = absolute_journal_path.to_owned();
    let new_shm = format!("{new_db_name}-shm");
    let new_wal = format!("{new_db_name}-wal");

    // Whenever there is an old db file, migrate it to the new db path.
    for (f, what) in [
        (&new_db_name, "db"),
        (&new_wal, "db WAL"),
        (&new_shm, "db SHM"),
    ] {
        if Path::new(f).exists()
            && let Err(e) = std::fs::remove_file(f)
        {
            warn!(target: LOG_TARGET, "Database migration: Could not remove {what} file {f} due to {e}");
            return false;
        }
    }
    for (from, to) in [
        (&old_db_name, &new_db_name),
        (&old_wal, &new_wal),
        (&old_shm, &new_shm),
    ] {
        if let Err(e) = std::fs::rename(from, to) {
            warn!(target: LOG_TARGET, "Database migration: could not rename {from} to {to}: {e}");
            return false;
        }
    }
    info!(target: LOG_TARGET, "Journal successfully migrated from {old_db_name} to {new_db_name}");
    true
}

/// Compare like `QString::operator<` (UTF-16 code units).
fn qstring_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Sort like `QStringList::sort()`.
pub(crate) fn qstring_sort(list: &mut [String]) {
    list.sort_by(|a, b| qstring_cmp(a, b));
}

/// `SyncJournalDb::findPathInSelectiveSyncList`: given a sorted list of paths
/// ending with `/`, return whether `path` is within one of them.
pub fn find_path_in_selective_sync_list(list: &[String], path: &str) -> bool {
    debug_assert!(list.windows(2).all(|w| qstring_cmp(&w[0], &w[1]).is_le()));

    if list.len() == 1 && list[0] == "/" {
        // Special case for the case "/" is there, it matches everything
        return true;
    }
    let path_slash = format!("{path}/");

    // Since the list is sorted, we can do a binary search.
    let it = list.partition_point(|e| qstring_cmp(e, &path_slash).is_lt());
    if it < list.len() && list[it] == path_slash {
        return true;
    }
    if it == 0 {
        return false;
    }
    let prev = &list[it - 1];
    debug_assert!(prev.ends_with('/'));
    path_slash.starts_with(prev.as_str())
}

// ---------------------------------------------------------------------------
// Value conversions mirroring ownsql.cpp's binders and getters.

/// Bind like a `QByteArray`: always TEXT (possibly empty), never NULL or BLOB.
struct BaText<'a>(&'a [u8]);

impl ToSql for BaText<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0)))
    }
}

/// Bind like a `QString`: TEXT, or NULL for a null string. A Rust `String`
/// has no null state; we treat the empty string as null since that is what
/// default-constructed upstream fields bind to.
struct QStr<'a>(&'a str);

impl ToSql for QStr<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        if self.0.is_empty() {
            Ok(ToSqlOutput::Owned(Value::Null))
        } else {
            Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0.as_bytes())))
        }
    }
}

/// `sqlite3_column_blob` semantics: NULL is empty, numbers are rendered as text.
fn ba_value(row: &Row<'_>, idx: usize) -> Vec<u8> {
    match row.get_ref(idx) {
        Ok(ValueRef::Text(t)) | Ok(ValueRef::Blob(t)) => t.to_vec(),
        Ok(ValueRef::Integer(i)) => i.to_string().into_bytes(),
        Ok(ValueRef::Real(f)) => f.to_string().into_bytes(),
        Ok(ValueRef::Null) | Err(_) => Vec::new(),
    }
}

fn string_value(row: &Row<'_>, idx: usize) -> String {
    String::from_utf8_lossy(&ba_value(row, idx)).into_owned()
}

/// Parse the leading integer of a text value like SQLite's text-to-integer
/// conversion does for `sqlite3_column_int64`.
fn text_to_i64(t: &[u8]) -> i64 {
    let s = std::str::from_utf8(t).unwrap_or("").trim_start();
    let bytes = s.as_bytes();
    let mut end = 0;
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if let Ok(v) = s[..end].parse::<i64>() {
        return v;
    }
    s.parse::<f64>().map(|f| f as i64).unwrap_or(0)
}

/// `sqlite3_column_int64` semantics.
fn int64_value(row: &Row<'_>, idx: usize) -> i64 {
    match row.get_ref(idx) {
        Ok(ValueRef::Integer(i)) => i,
        Ok(ValueRef::Real(f)) => f as i64,
        Ok(ValueRef::Text(t)) | Ok(ValueRef::Blob(t)) => text_to_i64(t),
        Ok(ValueRef::Null) | Err(_) => 0,
    }
}

/// `sqlite3_column_int` semantics (truncation to 32 bits).
fn int_value(row: &Row<'_>, idx: usize) -> i32 {
    int64_value(row, idx) as i32
}

fn is_null(row: &Row<'_>, idx: usize) -> bool {
    matches!(row.get_ref(idx), Ok(ValueRef::Null))
}

fn fill_file_record_from_get_query(row: &Row<'_>) -> SyncJournalFileRecord {
    SyncJournalFileRecord {
        path: ba_value(row, 0),
        inode: int64_value(row, 1) as u64,
        modtime: int64_value(row, 2),
        item_type: ItemType::from_db(int_value(row, 3).into()),
        etag: ba_value(row, 4),
        file_id: ba_value(row, 5),
        remote_perm: RemotePermissions::from_db_value(&ba_value(row, 6)),
        file_size: int64_value(row, 7),
        server_has_ignored_files: int_value(row, 8) > 0,
        checksum_header: ba_value(row, 9),
        e2e_mangled_name: ba_value(row, 10),
        e2e_encryption_status: JournalDbEncryptionStatus::from_db(int_value(row, 11).into()),
        lockstate: SyncJournalFileLockInfo {
            locked: int_value(row, 13) > 0,
            lock_owner_display_name: string_value(row, 14),
            lock_owner_id: string_value(row, 15),
            lock_owner_type: int64_value(row, 16),
            lock_editor_app: string_value(row, 17),
            lock_time: int64_value(row, 18),
            lock_timeout: int64_value(row, 19),
            lock_token: string_value(row, 20),
        },
        is_shared: int_value(row, 21) > 0,
        last_share_state_fetched_timestamp: int64_value(row, 22),
        shared_by_me: int_value(row, 23) > 0,
        is_live_photo: int_value(row, 24) > 0,
        live_photo_file: string_value(row, 25),
        folder_quota: FolderQuota {
            bytes_used: int64_value(row, 26),
            bytes_available: int64_value(row, 27),
        },
    }
}

fn to_download_info(row: &Row<'_>) -> DownloadInfo {
    DownloadInfo {
        tmpfile: string_value(row, 0),
        etag: ba_value(row, 1),
        error_count: int_value(row, 2),
        valid: true,
    }
}

/// Run a query and collect all rows (upstream's `Q_FOREVER { next() }` loop).
fn collect_rows<T>(
    stmt: &mut Statement<'_>,
    params: impl rusqlite::Params,
    mut f: impl FnMut(&Row<'_>) -> T,
) -> rusqlite::Result<Vec<T>> {
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(f(row));
    }
    Ok(out)
}

/// `parent_hash(path)`: `c_jhash64` of everything before the last `/`
/// (of the empty string for top-level items).
fn register_parent_hash(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "parent_hash",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // sqlite3_value_text(): numbers are rendered as text. Upstream
            // would crash on NULL; we return NULL.
            let text: Vec<u8> = match ctx.get_raw(0) {
                ValueRef::Null => return Ok(None),
                ValueRef::Text(t) | ValueRef::Blob(t) => t.to_vec(),
                ValueRef::Integer(i) => i.to_string().into_bytes(),
                ValueRef::Real(f) => f.to_string().into_bytes(),
            };
            // strrchr stops at the first NUL, like the C string it is.
            let text = match text.iter().position(|&c| c == 0) {
                Some(nul) => &text[..nul],
                None => &text[..],
            };
            let end = text.iter().rposition(|&c| c == b'/').unwrap_or(0);
            Ok(Some(c_jhash64(&text[..end], 0) as i64))
        },
    )
}

// ---------------------------------------------------------------------------

struct Inner {
    db: Option<Connection>,
    db_file: PathBuf,
    journal_mode: String,
    checksum_type_cache: HashMap<Vec<u8>, i64>,
    transaction: bool,
    metadata_table_is_empty: bool,
    /// Storing etags to these folders, or their parent folders, is filtered
    /// out (see [`SyncJournalDb::schedule_path_for_remote_discovery`]).
    /// The contained paths have a trailing `/`.
    etag_storage_filter: Vec<Vec<u8>>,
    autotest_fail_counter: i32,
}

/// `defaultJournalMode()`: WAL, except DELETE on FAT (Windows) and on
/// `/Volumes/` (macOS).
fn default_journal_mode(db_path: &Path) -> String {
    if cfg!(target_os = "macos") && db_path.starts_with("/Volumes/") {
        info!(target: LOG_TARGET, "Mounted sync dir, do not use WAL for {}", db_path.display());
        return "DELETE".to_owned();
    }
    // TODO(windows): FileSystem::fileSystemForPath() contains "FAT" => DELETE.
    "WAL".to_owned()
}

fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_owned(),
    }
}

/// `SqlDatabase::CheckDbResult`.
enum CheckDbResult {
    Ok,
    CantPrepare(rusqlite::Error),
    CantExec,
    NotOk,
}

fn sqlite_code(e: &rusqlite::Error) -> Option<(ffi::ErrorCode, i32)> {
    match e {
        rusqlite::Error::SqliteFailure(err, _) => Some((err.code, err.extended_code)),
        _ => None,
    }
}

fn open_helper(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(conn)
}

fn check_db(conn: &Connection) -> CheckDbResult {
    // quick_check can fail with a disk IO error when diskspace is low
    let mut stmt = match conn.prepare("PRAGMA quick_check;") {
        Ok(s) => s,
        Err(e) => {
            warn!(target: LOG_TARGET, "Error preparing quick_check on database");
            return CheckDbResult::CantPrepare(e);
        }
    };
    let result: rusqlite::Result<Option<String>> =
        stmt.query_row([], |r| Ok(Some(string_value(r, 0))));
    match result {
        Ok(Some(r)) if r == "ok" => CheckDbResult::Ok,
        Ok(r) => {
            warn!(target: LOG_TARGET, "quick_check returned failure: {r:?}");
            CheckDbResult::NotOk
        }
        Err(_) => {
            warn!(target: LOG_TARGET, "Error running quick_check on database");
            CheckDbResult::CantExec
        }
    }
}

/// `SqlDatabase::openOrCreateReadWrite`.
fn open_or_create_read_write(path: &Path) -> rusqlite::Result<Connection> {
    let conn = open_helper(path)?;
    match check_db(&conn) {
        CheckDbResult::Ok => Ok(conn),
        result => {
            if let CheckDbResult::CantPrepare(e) = result {
                // When disk space is low, preparing may fail even though the db is fine.
                // TODO: upstream also gives up when free disk space < 1 MB.
                // Even when there's enough disk space, it might very well be that the
                // file is on a read-only filesystem and can't be opened because of that.
                if matches!(sqlite_code(&e), Some((ffi::ErrorCode::CannotOpen, _))) {
                    warn!(target: LOG_TARGET, "Can't open db to prepare consistency check, aborting");
                    return Err(e);
                }
            }
            error!(target: LOG_TARGET, "Consistency check failed, removing broken db {}", path.display());
            drop(conn);
            let _ = std::fs::remove_file(path);
            open_helper(path)
        }
    }
}

impl Inner {
    fn conn(&self) -> &Connection {
        self.db.as_ref().expect("journal database is open")
    }

    fn start_transaction(&mut self) {
        if !self.transaction {
            let Some(db) = &self.db else { return };
            if let Err(e) = db.execute_batch("BEGIN") {
                warn!(target: LOG_TARGET, "ERROR starting transaction: {e}");
                return;
            }
            self.transaction = true;
        } else {
            debug!(target: LOG_TARGET, "Database Transaction is running, not starting another one!");
        }
    }

    fn commit_transaction(&mut self) {
        if self.transaction {
            let Some(db) = &self.db else { return };
            if let Err(e) = db.execute_batch("COMMIT") {
                warn!(target: LOG_TARGET, "ERROR committing to the database: {e}");
                return;
            }
            self.transaction = false;
        } else {
            debug!(target: LOG_TARGET, "No database Transaction to commit");
        }
    }

    fn commit_internal(&mut self, context: &str, start_trans: bool) {
        debug!(
            target: LOG_TARGET,
            "Transaction commit {context}{}",
            if start_trans { " and starting new transaction" } else { "" }
        );
        self.commit_transaction();
        if start_trans {
            self.start_transaction();
        }
    }

    /// Closes the SQLite connection only (`SqlDatabase::close`).
    fn close_db(&mut self) {
        if let Some(db) = self.db.take()
            && let Err((_, e)) = db.close()
        {
            warn!(target: LOG_TARGET, "Closing database failed {e}");
        }
        // The transaction died with the connection.
        self.transaction = false;
    }

    /// `SyncJournalDb::close` (without locking).
    fn close(&mut self) {
        info!(target: LOG_TARGET, "Closing DB {}", self.db_file.display());
        self.commit_transaction();
        self.close_db();
        self.etag_storage_filter.clear();
        self.metadata_table_is_empty = false;
    }

    fn sql_fail(&mut self, log: &str, err: &dyn std::fmt::Display) -> bool {
        self.commit_transaction();
        warn!(target: LOG_TARGET, "SQL Error {log} {err}");
        self.close_db();
        // Upstream ASSERT(false): fatal in debug builds of the client only.
        false
    }

    fn check_connect(&mut self) -> bool {
        if self.autotest_fail_counter >= 0 {
            let was = self.autotest_fail_counter;
            self.autotest_fail_counter -= 1;
            if was == 0 {
                info!(target: LOG_TARGET, "Error Simulated");
                return false;
            }
        }

        if self.db.is_some() {
            // Unfortunately the sqlite isOpen check can return true even when the underlying
            // storage has become unavailable - and then some operations may cause crashes.
            if !self.db_file.exists() {
                warn!(target: LOG_TARGET, "Database open, but file {} does not exist", self.db_file.display());
                self.close();
                return false;
            }
            return true;
        }

        if self.db_file.as_os_str().is_empty() {
            warn!(target: LOG_TARGET, "Database filename is empty");
            return false;
        }

        // The database file is created by this call (SQLITE_OPEN_CREATE)
        let conn = match open_or_create_read_write(&self.db_file) {
            Ok(c) => c,
            Err(e) => {
                warn!(target: LOG_TARGET, "Error opening the db: {e}");
                return false;
            }
        };
        self.db = Some(conn);

        if !self.db_file.exists() {
            warn!(target: LOG_TARGET, "Database file {} does not exist", self.db_file.display());
            return false;
        }

        match self
            .conn()
            .query_row("SELECT sqlite_version();", [], |r| Ok(string_value(r, 0)))
        {
            Ok(v) => info!(target: LOG_TARGET, "sqlite3 version {v}"),
            Err(e) => return self.sql_fail("SELECT sqlite_version()", &e),
        }

        // Set locking mode to avoid issues with WAL on Windows
        let locking_mode = env_or("OWNCLOUD_SQLITE_LOCKING_MODE", "EXCLUSIVE");
        match self
            .conn()
            .query_row(&format!("PRAGMA locking_mode={locking_mode};"), [], |r| {
                Ok(string_value(r, 0))
            }) {
            Ok(v) => info!(target: LOG_TARGET, "sqlite3 locking_mode= {v}"),
            Err(e) => return self.sql_fail("Set PRAGMA locking_mode", &e),
        }

        match self.conn().query_row(
            &format!("PRAGMA journal_mode={};", self.journal_mode),
            [],
            |r| Ok(string_value(r, 0)),
        ) {
            Ok(v) => info!(target: LOG_TARGET, "sqlite3 journal_mode= {v}"),
            Err(e) => return self.sql_fail("Set PRAGMA journal_mode", &e),
        }

        // Upstream prepares "PRAGMA temp_store = $OWNCLOUD_SQLITE_TEMP_STORE",
        // "PRAGMA synchronous = NORMAL|FULL" and "PRAGMA case_sensitive_like = ON"
        // here but only calls SqlQuery::exec() on them, which does not step
        // PRAGMA statements: they never take effect. We only check that they
        // would prepare, keeping SQLite's defaults exactly like upstream.
        if let Ok(temp_store) = std::env::var("OWNCLOUD_SQLITE_TEMP_STORE")
            && !temp_store.is_empty()
            && let Err(e) = self
                .conn()
                .prepare(&format!("PRAGMA temp_store = {temp_store};"))
                .map(drop)
        {
            return self.sql_fail("Set PRAGMA temp_store", &e);
        }
        let synchronous_mode = if self.journal_mode.eq_ignore_ascii_case("wal") {
            "NORMAL"
        } else {
            "FULL"
        };
        if let Err(e) = self
            .conn()
            .prepare(&format!("PRAGMA synchronous = {synchronous_mode};"))
            .map(drop)
        {
            return self.sql_fail("Set PRAGMA synchronous", &e);
        }
        if let Err(e) = self
            .conn()
            .prepare("PRAGMA case_sensitive_like = ON;")
            .map(drop)
        {
            return self.sql_fail("Set PRAGMA case_sensitivity", &e);
        }

        if let Err(e) = register_parent_hash(self.conn()) {
            return self.sql_fail("create function parent_hash", &e);
        }

        // Because insert is so slow, we do everything in a transaction, and only need one call to commit
        self.start_transaction();

        let create_metadata = "CREATE TABLE IF NOT EXISTS metadata(\
                               phash INTEGER(8),\
                               pathlen INTEGER,\
                               path VARCHAR(4096),\
                               inode INTEGER,\
                               uid INTEGER,\
                               gid INTEGER,\
                               mode INTEGER,\
                               modtime INTEGER(8),\
                               type INTEGER,\
                               md5 VARCHAR(32),\
                               PRIMARY KEY(phash)\
                               );";
        if let Err(e) = self.conn().execute(create_metadata, []) {
            // In certain situations the io error can be avoided by switching
            // to the DELETE journal mode, see #5723
            if self.journal_mode != "DELETE"
                && matches!(sqlite_code(&e), Some((ffi::ErrorCode::SystemIoFailure, ext)) if ext == ffi::SQLITE_IOERR_SHMMAP)
            {
                warn!(target: LOG_TARGET, "IO error SHMMAP on table creation, attempting with DELETE journal mode");
                self.journal_mode = "DELETE".to_owned();
                self.commit_transaction();
                self.close_db();
                return self.check_connect();
            }
            return self.sql_fail("Create table metadata", &e);
        }

        let tables: [(&str, &str); 12] = [
            (
                "key_value_store",
                "CREATE TABLE IF NOT EXISTS key_value_store(key VARCHAR(4096), value VARCHAR(4096), PRIMARY KEY(key));",
            ),
            (
                "downloadinfo",
                "CREATE TABLE IF NOT EXISTS downloadinfo(\
                 path VARCHAR(4096),\
                 tmpfile VARCHAR(4096),\
                 etag VARCHAR(32),\
                 errorcount INTEGER,\
                 PRIMARY KEY(path)\
                 );",
            ),
            (
                "uploadinfo",
                "CREATE TABLE IF NOT EXISTS uploadinfo(\
                 path VARCHAR(4096),\
                 chunk INTEGER,\
                 transferid INTEGER,\
                 errorcount INTEGER,\
                 size INTEGER(8),\
                 modtime INTEGER(8),\
                 contentChecksum TEXT,\
                 PRIMARY KEY(path)\
                 );",
            ),
            (
                "blacklist",
                "CREATE TABLE IF NOT EXISTS blacklist (\
                 path VARCHAR(4096),\
                 lastTryEtag VARCHAR[32],\
                 lastTryModtime INTEGER[8],\
                 retrycount INTEGER,\
                 errorstring VARCHAR[4096],\
                 PRIMARY KEY(path)\
                 );",
            ),
            (
                "async_poll",
                "CREATE TABLE IF NOT EXISTS async_poll(\
                 path VARCHAR(4096),\
                 modtime INTEGER(8),\
                 filesize BIGINT,\
                 pollpath VARCHAR(4096));",
            ),
            (
                "selectivesync",
                "CREATE TABLE IF NOT EXISTS selectivesync (\
                 path VARCHAR(4096),\
                 type INTEGER\
                 );",
            ),
            (
                "checksumtype",
                "CREATE TABLE IF NOT EXISTS checksumtype(\
                 id INTEGER PRIMARY KEY,\
                 name TEXT UNIQUE\
                 );",
            ),
            (
                "datafingerprint",
                "CREATE TABLE IF NOT EXISTS datafingerprint(\
                 fingerprint TEXT UNIQUE\
                 );",
            ),
            (
                "flags",
                "CREATE TABLE IF NOT EXISTS flags (\
                 path TEXT PRIMARY KEY,\
                 pinState INTEGER\
                 );",
            ),
            (
                "conflicts",
                "CREATE TABLE IF NOT EXISTS conflicts(\
                 path TEXT PRIMARY KEY,\
                 baseFileId TEXT,\
                 baseEtag TEXT,\
                 baseModtime INTEGER\
                 );",
            ),
            (
                "caseconflicts",
                "CREATE TABLE IF NOT EXISTS caseconflicts(\
                 path TEXT PRIMARY KEY,\
                 baseFileId TEXT,\
                 baseEtag TEXT,\
                 baseModtime INTEGER,\
                 basePath TEXT UNIQUE\
                 );",
            ),
            (
                "version",
                "CREATE TABLE IF NOT EXISTS version(\
                 major INTEGER(8),\
                 minor INTEGER(8),\
                 patch INTEGER(8),\
                 custom VARCHAR(256)\
                 );",
            ),
        ];
        for (name, sql) in tables {
            if let Err(e) = self.conn().execute(sql, []) {
                return self.sql_fail(&format!("Create table {name}"), &e);
            }
        }
        if let Err(e) = self.conn().execute(
            "CREATE TABLE IF NOT EXISTS e2EeLockedFolders(\
             folderId VARCHAR(128) PRIMARY KEY,\
             token VARCHAR(4096)\
             );",
            [],
        ) {
            return self.sql_fail("Create table e2EeLockedFolders", &e);
        }

        let mut force_remote_discovery = false;

        let version = self
            .conn()
            .query_row("SELECT major, minor, patch FROM version;", [], |r| {
                Ok((int_value(r, 0), int_value(r, 1), int_value(r, 2)))
            });
        let (cur_major, cur_minor, cur_patch, cur_build) = CLIENT_VERSION;
        match version {
            Err(rusqlite::Error::QueryReturnedNoRows) | Err(_) => {
                force_remote_discovery = true;
                if let Err(e) = self.conn().execute(
                    "INSERT INTO version VALUES (?1, ?2, ?3, ?4);",
                    rusqlite::params![cur_major, cur_minor, cur_patch, cur_build as i64],
                ) {
                    return self.sql_fail("Update version", &e);
                }
            }
            Ok((major, minor, patch)) => {
                let (major, minor, patch) = (i64::from(major), i64::from(minor), i64::from(patch));
                if major == 1 && minor == 8 && (patch == 0 || patch == 1) {
                    info!(target: LOG_TARGET, "possibleUpgradeFromMirall_1_8_0_or_1 detected!");
                    force_remote_discovery = true;
                }
                // There was a bug in versions <2.3.0 that could lead to stale
                // local files and a remote discovery will fix them.
                if major == 2 && minor < 3 {
                    info!(target: LOG_TARGET, "upgrade form client < 2.3.0 detected! forcing remote discovery");
                    force_remote_discovery = true;
                }
                // Not comparing the BUILD id here, correct?
                if !(major == cur_major && minor == cur_minor && patch == cur_patch) {
                    if let Err(e) = self.conn().execute(
                        "UPDATE version SET major=?1, minor=?2, patch =?3, custom=?4 \
                         WHERE major=?5 AND minor=?6 AND patch=?7;",
                        rusqlite::params![
                            cur_major,
                            cur_minor,
                            cur_patch,
                            cur_build as i64,
                            major,
                            minor,
                            patch
                        ],
                    ) {
                        return self.sql_fail("Update version", &e);
                    }
                    if (major < MAJOR_VERSION_3
                        || (major == MAJOR_VERSION_3 && minor <= MINOR_VERSION_16))
                        && !self.ensure_correct_encryption_status()
                    {
                        warn!(target: LOG_TARGET, "Failed to update the encryption status");
                    }
                }
            }
        }

        self.commit_internal("checkConnect", true);

        let rc = self.update_database_structure();
        if !rc {
            warn!(target: LOG_TARGET, "Failed to update the database structure!");
        }

        // If we are upgrading from a client version older than 1.5, we cannot read
        // from the database because we need to fetch the files id and etags.
        if force_remote_discovery {
            self.force_remote_discovery_next_sync_locked();
        }
        if self.db.is_none() {
            return false;
        }

        // Upstream prepares (and caches) these statements here and fails the
        // connection if they don't prepare.
        let blacklist_sql = Self::blacklist_query_sql();
        for (name, sql) in [
            (
                "_deleteDownloadInfoQuery",
                "DELETE FROM downloadinfo WHERE path=?1",
            ),
            (
                "_deleteUploadInfoQuery",
                "DELETE FROM uploadinfo WHERE path=?1",
            ),
            ("_getErrorBlacklistQuery", blacklist_sql.as_str()),
        ] {
            if let Err(e) = self.conn().prepare_cached(sql).map(drop) {
                warn!(target: LOG_TARGET, "database error: {e}");
                return self.sql_fail(&format!("prepare {name}"), &e);
            }
        }

        // don't start a new transaction now
        self.commit_internal("checkConnect End", false);

        // This avoid reading from the DB if we already know it is empty
        // thereby speeding up the initial discovery significantly.
        self.metadata_table_is_empty = self.get_file_record_count() == 0;

        // FileSystem::setFileHidden() is a no-op outside Windows.
        rc
    }

    fn blacklist_query_sql() -> String {
        let mut sql = String::from(
            "SELECT lastTryEtag, lastTryModtime, retrycount, errorstring, lastTryTime, ignoreDuration, renameTarget, errorCategory, requestId \
             FROM blacklist WHERE path=?1",
        );
        if utility::fs_case_preserving() {
            // if the file system is case preserving we have to check the blacklist
            // case insensitively
            sql.push_str(" COLLATE NOCASE");
        }
        sql
    }

    fn update_database_structure(&mut self) -> bool {
        if !self.update_metadata_table_structure() {
            return false;
        }
        if !self.update_error_blacklist_table_structure() {
            return false;
        }
        true
    }

    fn exec_sql(&self, sql: &str) -> rusqlite::Result<()> {
        match &self.db {
            Some(db) => db.execute_batch(sql),
            None => Err(rusqlite::Error::InvalidQuery),
        }
    }

    fn has_default_value(&mut self, column_name: &str) -> bool {
        let sql = format!(
            "SELECT dflt_value FROM pragma_table_info('metadata') WHERE name = '{column_name}';"
        );
        let Some(db) = &self.db else { return false };
        let result = db.query_row(&sql, [], |r| Ok(is_null(r, 0)));
        match result {
            Ok(null) => !null,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                warn!(target: LOG_TARGET, "database error: no default value row for {column_name}");
                false
            }
            Err(e) => {
                self.sql_fail(&format!("check default value for: {column_name}"), &e);
                false
            }
        }
    }

    fn remove_column(&mut self, column_name: &str) -> bool {
        if let Err(e) = self.exec_sql(&format!("ALTER TABLE metadata DROP COLUMN {column_name};")) {
            self.sql_fail(
                &format!("update metadata structure: drop {column_name} column"),
                &e,
            );
            return false;
        }
        self.commit_internal(
            &format!("update database structure: drop {column_name} column"),
            true,
        );
        true
    }

    /// The `addColumn` lambda of `updateMetadataTableStructure`.
    fn add_column(
        &mut self,
        columns: &[String],
        column_name: &str,
        data_type: &str,
        with_index: bool,
        default_command: &str,
    ) -> bool {
        if columns.iter().any(|c| c == column_name) {
            return true;
        }
        let mut re = true;
        let mut request = format!("ALTER TABLE metadata ADD COLUMN {column_name} {data_type}");
        if !default_command.is_empty() {
            request.push(' ');
            request.push_str(default_command);
        }
        request.push(';');
        if let Err(e) = self.exec_sql(&request) {
            self.sql_fail(
                &format!("updateMetadataTableStructure: add {column_name} column"),
                &e,
            );
            re = false;
        }
        if with_index
            && let Err(e) = self.exec_sql(&format!(
                "CREATE INDEX metadata_{column_name} ON metadata({column_name});"
            ))
        {
            self.sql_fail(
                &format!("updateMetadataTableStructure: create index {column_name}"),
                &e,
            );
            re = false;
        }
        self.commit_internal(
            &format!("update database structure: add {column_name} column"),
            true,
        );
        re
    }

    fn update_metadata_table_structure(&mut self) -> bool {
        let mut columns = self.table_columns("metadata");
        let mut re = true;

        // check if the file_id column is there and create it if not
        if columns.is_empty() {
            return false;
        }

        re &= self.add_column(&columns, "fileid", "VARCHAR(128)", true, "");
        re &= self.add_column(&columns, "remotePerm", "VARCHAR(128)", false, "");
        re &= self.add_column(&columns, "filesize", "BIGINT", false, "");

        for (sql, what, ctx) in [
            (
                "CREATE INDEX IF NOT EXISTS metadata_inode ON metadata(inode);",
                "create index inode",
                "add inode index",
            ),
            (
                "CREATE INDEX IF NOT EXISTS metadata_path ON metadata(path);",
                "create index path",
                "add path index",
            ),
            (
                "CREATE INDEX IF NOT EXISTS metadata_parent ON metadata(parent_hash(path));",
                "create index parent",
                "add parent index",
            ),
        ] {
            if let Err(e) = self.exec_sql(sql) {
                self.sql_fail(&format!("updateMetadataTableStructure: {what}"), &e);
                re = false;
            }
            self.commit_internal(&format!("update database structure: {ctx}"), true);
        }

        re &= self.add_column(&columns, "ignoredChildrenRemote", "INT", false, "");
        re &= self.add_column(&columns, "contentChecksum", "TEXT", false, "");
        re &= self.add_column(&columns, "contentChecksumTypeId", "INTEGER", false, "");
        re &= self.add_column(&columns, "e2eMangledName", "TEXT", false, "");
        re &= self.add_column(&columns, "isE2eEncrypted", "INTEGER", false, "");
        re &= self.add_column(&columns, "e2eCertificateFingerprint", "TEXT", false, "");
        re &= self.add_column(&columns, "isShared", "INTEGER", false, "");
        re &= self.add_column(
            &columns,
            "lastShareStateFetchedTimestmap",
            "INTEGER",
            false,
            "",
        );
        re &= self.add_column(&columns, "sharedByMe", "INTEGER", false, "");

        let upload_info_columns = self.table_columns("uploadinfo");
        if upload_info_columns.is_empty() {
            return false;
        }
        if !upload_info_columns.iter().any(|c| c == "contentChecksum") {
            if let Err(e) = self.exec_sql("ALTER TABLE uploadinfo ADD COLUMN contentChecksum TEXT;")
            {
                self.sql_fail(
                    "updateMetadataTableStructure: add contentChecksum column",
                    &e,
                );
                re = false;
            }
            self.commit_internal(
                "update database structure: add contentChecksum col for uploadinfo",
                true,
            );
        }

        let conflicts_columns = self.table_columns("conflicts");
        if conflicts_columns.is_empty() {
            return false;
        }
        if !conflicts_columns.iter().any(|c| c == "basePath")
            && let Err(e) = self.exec_sql("ALTER TABLE conflicts ADD COLUMN basePath TEXT;")
        {
            self.sql_fail("updateMetadataTableStructure: add basePath column", &e);
            re = false;
        }

        if let Err(e) =
            self.exec_sql("CREATE INDEX IF NOT EXISTS metadata_e2e_id ON metadata(e2eMangledName);")
        {
            self.sql_fail(
                "updateMetadataTableStructure: create index e2eMangledName",
                &e,
            );
            re = false;
        }
        if let Err(e) = self.exec_sql(
            "CREATE INDEX IF NOT EXISTS metadata_e2e_status ON metadata (path, phash, type, isE2eEncrypted)",
        ) {
            self.sql_fail("updateMetadataTableStructure: create index metadata_e2e_status", &e);
            re = false;
        }
        self.commit_internal("update database structure: add e2eMangledName index", true);

        re &= self.add_column(&columns, "lock", "INTEGER", false, "");
        re &= self.add_column(&columns, "lockType", "INTEGER", false, "");
        re &= self.add_column(&columns, "lockOwnerDisplayName", "TEXT", false, "");
        re &= self.add_column(&columns, "lockOwnerId", "TEXT", false, "");
        re &= self.add_column(&columns, "lockOwnerEditor", "TEXT", false, "");
        re &= self.add_column(&columns, "lockTime", "INTEGER", false, "");
        re &= self.add_column(&columns, "lockTimeout", "INTEGER", false, "");
        re &= self.add_column(&columns, "lockToken", "TEXT", false, "");

        if let Err(e) = self.exec_sql(
            "CREATE INDEX IF NOT EXISTS caseconflicts_basePath ON caseconflicts(basePath);",
        ) {
            self.sql_fail("caseconflictsTableStructure: create index basePath", &e);
            return false;
        }
        self.commit_internal("update database structure: add basePath index", true);

        re &= self.add_column(&columns, "isLivePhoto", "INTEGER", false, "");
        re &= self.add_column(&columns, "livePhotoFile", "TEXT", false, "");

        {
            let quota_bytes_used = "quotaBytesUsed";
            let quota_bytes_available = "quotaBytesAvailable";
            let default_command = "DEFAULT -1 NOT NULL";
            let big_int = "BIGINT";
            let mut result = false;

            if columns.iter().any(|c| c == quota_bytes_used)
                && !self.has_default_value(quota_bytes_used)
            {
                result = self.remove_column(quota_bytes_used);
            }
            if columns.iter().any(|c| c == quota_bytes_available)
                && !self.has_default_value(quota_bytes_available)
            {
                result = self.remove_column(quota_bytes_available);
            }
            if result {
                columns = self.table_columns("metadata");
            }
            re &= self.add_column(&columns, quota_bytes_used, big_int, false, default_command);
            re &= self.add_column(
                &columns,
                quota_bytes_available,
                big_int,
                false,
                default_command,
            );
        }

        re
    }

    fn update_error_blacklist_table_structure(&mut self) -> bool {
        let columns = self.table_columns("blacklist");
        let mut re = true;

        if columns.is_empty() {
            return false;
        }
        let has = |name: &str| columns.iter().any(|c| c == name);

        if !has("lastTryTime") {
            if let Err(e) =
                self.exec_sql("ALTER TABLE blacklist ADD COLUMN lastTryTime INTEGER(8);")
            {
                self.sql_fail("updateBlacklistTableStructure: Add lastTryTime fileid", &e);
                re = false;
            }
            if let Err(e) =
                self.exec_sql("ALTER TABLE blacklist ADD COLUMN ignoreDuration INTEGER(8);")
            {
                self.sql_fail(
                    "updateBlacklistTableStructure: Add ignoreDuration fileid",
                    &e,
                );
                re = false;
            }
            self.commit_internal(
                "update database structure: add lastTryTime, ignoreDuration cols",
                true,
            );
        }
        if !has("renameTarget") {
            if let Err(e) =
                self.exec_sql("ALTER TABLE blacklist ADD COLUMN renameTarget VARCHAR(4096);")
            {
                self.sql_fail("updateBlacklistTableStructure: Add renameTarget", &e);
                re = false;
            }
            self.commit_internal("update database structure: add renameTarget col", true);
        }
        if !has("errorCategory") {
            if let Err(e) =
                self.exec_sql("ALTER TABLE blacklist ADD COLUMN errorCategory INTEGER(8);")
            {
                self.sql_fail("updateBlacklistTableStructure: Add errorCategory", &e);
                re = false;
            }
            self.commit_internal("update database structure: add errorCategory col", true);
        }
        if !has("requestId") {
            if let Err(e) = self.exec_sql("ALTER TABLE blacklist ADD COLUMN requestId VARCHAR(36);")
            {
                self.sql_fail("updateBlacklistTableStructure: Add requestId", &e);
                re = false;
            }
            self.commit_internal("update database structure: add errorCategory col", true);
        }

        if let Err(e) = self.exec_sql(
            "CREATE INDEX IF NOT EXISTS blacklist_index ON blacklist(path collate nocase);",
        ) {
            self.sql_fail(
                "updateErrorBlacklistTableStructure: create index blacklit",
                &e,
            );
            re = false;
        }
        re
    }

    fn table_columns(&mut self, table: &str) -> Vec<String> {
        if !self.check_connect() {
            return Vec::new();
        }
        let Ok(mut stmt) = self
            .conn()
            .prepare(&format!("PRAGMA table_info('{table}');"))
        else {
            return Vec::new();
        };
        let columns = collect_rows(&mut stmt, [], |r| string_value(r, 1)).unwrap_or_default();
        debug!(target: LOG_TARGET, "Columns in the current journal: {columns:?}");
        columns
    }

    fn get_file_record_count(&self) -> i64 {
        let Some(db) = &self.db else { return -1 };
        db.query_row("SELECT COUNT(*) FROM metadata", [], |r| {
            Ok(int64_value(r, 0))
        })
        .unwrap_or(-1)
    }

    fn ensure_correct_encryption_status(&mut self) -> bool {
        info!(target: LOG_TARGET, "migration: ensure proper encryption status in database");
        let sql = "UPDATE \
                   metadata AS invalidItem \
                   SET \
                   isE2eEncrypted = 4 \
                   WHERE \
                   invalidItem.isE2eEncrypted = 0 AND \
                   (invalidItem.type = 0 OR invalidItem.type = 2) AND \
                   EXISTS ( \
                   SELECT * \
                   FROM \
                   metadata parentFolder \
                   WHERE \
                   parent_hash(invalidItem.path) = parentFolder.phash AND \
                   parentFolder.isE2eEncrypted <> 0)";
        match self
            .conn()
            .prepare_cached(sql)
            .and_then(|mut s| s.execute([]))
        {
            Ok(_) => true,
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                false
            }
        }
    }

    fn force_remote_discovery_next_sync_locked(&mut self) {
        info!(target: LOG_TARGET, "Forcing remote re-discovery by deleting folder Etags");
        if let Err(e) = self.exec_sql("UPDATE metadata SET md5='_invalid_' WHERE type=2;") {
            self.sql_fail("forceRemoteDiscoveryNextSyncLocked", &e);
        }
    }

    /// Returns the integer id of the checksum type; 0 on failure and for
    /// empty checksum types.
    fn map_checksum_type(&mut self, checksum_type: &[u8]) -> i64 {
        if checksum_type.is_empty() {
            return 0;
        }
        if let Some(&id) = self.checksum_type_cache.get(checksum_type) {
            return id;
        }
        // Ensure the checksum type is in the db
        if let Err(e) = self
            .conn()
            .prepare_cached("INSERT OR IGNORE INTO checksumtype (name) VALUES (?1)")
            .and_then(|mut s| s.execute([BaText(checksum_type)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
            return 0;
        }
        // Retrieve the id
        let id = self
            .conn()
            .prepare_cached("SELECT id FROM checksumtype WHERE name=?1")
            .and_then(|mut s| s.query_row([BaText(checksum_type)], |r| Ok(int_value(r, 0))));
        match id {
            Ok(id) => {
                let id = i64::from(id);
                self.checksum_type_cache.insert(checksum_type.to_vec(), id);
                id
            }
            Err(e) => {
                warn!(target: LOG_TARGET, "No checksum type mapping found for {:?}: {e}", String::from_utf8_lossy(checksum_type));
                0
            }
        }
    }

    fn schedule_path_for_remote_discovery(&mut self, file_name: &[u8]) {
        if !self.check_connect() {
            return;
        }
        // Remove trailing slash
        let mut argument = file_name.to_vec();
        if argument.ends_with(b"/") {
            argument.pop();
        }
        // This query will match entries for which the path is a prefix of fileName
        // Note: CSYNC_FTW_TYPE_DIR == 2
        let sql = concat!(
            "UPDATE metadata SET md5='_invalid_' WHERE ",
            is_prefix_path_or_equal!("path", "?1"),
            " AND type == 2;"
        );
        if let Err(e) = self.conn().execute(sql, [BaText(&argument)]) {
            self.sql_fail(
                &format!(
                    "schedulePathForRemoteDiscovery path: {}",
                    String::from_utf8_lossy(file_name)
                ),
                &e,
            );
        }
        // Prevent future overwrite of the etags of this folder and all
        // parent folders for this sync
        argument.push(b'/');
        self.etag_storage_filter.push(argument);
    }

    fn get_selective_sync_list(&mut self, ty: SelectiveSyncListType) -> Result<Vec<String>> {
        if !self.check_connect() {
            return Err(JournalError::Connect);
        }
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT path FROM selectivesync WHERE type=?1")?;
        let rows = collect_rows(&mut stmt, [ty as i32], |r| {
            utility::trailing_slash_path(&string_value(r, 0))
        })?;
        Ok(rows)
    }

    fn set_selective_sync_list(&mut self, ty: SelectiveSyncListType, list: &[String]) {
        if !self.check_connect() {
            return;
        }
        self.start_transaction();

        //first, delete all entries of this type
        if let Err(e) = self
            .conn()
            .execute("DELETE FROM selectivesync WHERE type == ?1", [ty as i32])
        {
            warn!(target: LOG_TARGET, "SQL error when deleting selective sync list {list:?} {e}");
        }
        for path in list {
            if let Err(e) = self.conn().execute(
                "INSERT INTO selectivesync VALUES (?1, ?2)",
                rusqlite::params![QStr(path), ty as i32],
            ) {
                warn!(target: LOG_TARGET, "SQL error when inserting into selective sync {ty:?} {path} {e}");
            }
        }
        self.commit_internal("setSelectiveSyncList", true);
    }

    fn get_file_records_by_file_id(
        &mut self,
        file_id: &[u8],
    ) -> Result<Vec<SyncJournalFileRecord>> {
        if file_id.is_empty() || self.metadata_table_is_empty {
            return Ok(Vec::new()); // no error, yet nothing found
        }
        if !self.check_connect() {
            return Err(JournalError::Connect);
        }
        let mut stmt = self
            .conn()
            .prepare_cached(concat!(get_file_record_query!(), " WHERE fileid=?1"))?;
        Ok(collect_rows(
            &mut stmt,
            [BaText(file_id)],
            fill_file_record_from_get_query,
        )?)
    }

    fn effective_pin_for_path(&mut self, path: &[u8]) -> Option<PinState> {
        if !self.check_connect() {
            return None;
        }
        let sql = concat!(
            "SELECT pinState FROM flags WHERE",
            // explicitly allow "" to represent the root path
            // (it'd be great if paths started with a / and "/" could be the root)
            " (",
            is_prefix_path_or_equal!("path", "?1"),
            " OR path == '')",
            " AND pinState is not null AND pinState != 0",
            " ORDER BY length(path) DESC LIMIT 1;"
        );
        let result = self
            .conn()
            .prepare_cached(sql)
            .and_then(|mut s| collect_rows(&mut s, [BaText(path)], |r| int64_value(r, 0)));
        match result {
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                None
            }
            // If the root path has no setting, assume AlwaysLocal
            Ok(rows) => Some(
                rows.first()
                    .map_or(PinState::AlwaysLocal, |&v| PinState::from_db(v)),
            ),
        }
    }
}

/// The sync journal database (`SyncJournalDb`).
///
/// Thread safe: all public functions lock an internal mutex.
pub struct SyncJournalDb {
    inner: Mutex<Inner>,
    db_file: PathBuf,
}

impl SyncJournalDb {
    /// Creates a journal object for `db_file_path`. The database is opened
    /// lazily by the first operation (or [`SyncJournalDb::open`]).
    ///
    /// The journal mode is WAL unless `OWNCLOUD_SQLITE_JOURNAL_MODE` is set.
    pub fn new(db_file_path: impl Into<PathBuf>) -> Self {
        let db_file = db_file_path.into();
        // Allow forcing the journal mode for debugging
        let mut journal_mode = std::env::var("OWNCLOUD_SQLITE_JOURNAL_MODE").unwrap_or_default();
        if journal_mode.is_empty() {
            journal_mode = default_journal_mode(&db_file);
        }
        Self {
            inner: Mutex::new(Inner {
                db: None,
                db_file: db_file.clone(),
                journal_mode,
                checksum_type_cache: HashMap::new(),
                transaction: false,
                metadata_table_is_empty: false,
                etag_storage_filter: Vec::new(),
                autotest_fail_counter: -1,
            }),
            db_file,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Only used for auto-tests: when non-negative, the counter is decreased
    /// for every database operation and reaching 0 makes the operation fail.
    pub fn set_autotest_fail_counter(&self, value: i32) {
        self.lock().autotest_fail_counter = value;
    }

    pub fn autotest_fail_counter(&self) -> i32 {
        self.lock().autotest_fail_counter
    }

    pub fn exists(&self) -> bool {
        let _l = self.lock();
        !self.db_file.as_os_str().is_empty() && self.db_file.exists()
    }

    pub fn database_file_path(&self) -> &Path {
        &self.db_file
    }

    /// Open the db if it isn't already. Returns whether it could be opened or
    /// is currently opened.
    pub fn open(&self) -> bool {
        self.lock().check_connect()
    }

    pub fn is_open(&self) -> bool {
        self.lock().db.is_some()
    }

    /// Close the database.
    pub fn close(&self) {
        self.lock().close();
    }

    /// Note that this does not change the size of the -wal file.
    ///
    /// Upstream only `exec()`s this PRAGMA, which never steps it (see the
    /// module documentation), and nothing calls it. Kept as a no-op.
    pub fn wal_checkpoint(&self) {}

    /// Because SQLite transactions are really slow, everything is
    /// encapsulated in big transactions. Commit will actually commit the
    /// transaction and create a new one.
    pub fn commit(&self, context: &str, start_trans: bool) {
        self.lock().commit_internal(context, start_trans);
    }

    pub fn commit_if_needed_and_start_new_transaction(&self, context: &str) {
        let mut inner = self.lock();
        if inner.transaction {
            inner.commit_internal(context, true);
        } else {
            inner.start_transaction();
        }
    }

    // ---- metadata --------------------------------------------------------

    /// Store a new or updated record in the database.
    pub fn set_file_record(&self, record: &SyncJournalFileRecord) -> Result<()> {
        let mut record = record.clone();
        let mut inner = self.lock();

        debug_assert!(record.modtime > 0);
        if record.modtime <= 0 {
            error!(target: LOG_TARGET, "invalid modification time");
        }

        if !inner.etag_storage_filter.is_empty() {
            // If we are a directory that should not be read from db next time, don't write the etag
            let mut prefix = record.path.clone();
            prefix.push(b'/');
            if let Some(it) = inner
                .etag_storage_filter
                .iter()
                .find(|it| it.starts_with(&prefix))
            {
                info!(
                    target: LOG_TARGET,
                    "Filtered writing the etag of {} because it is a prefix of {}",
                    String::from_utf8_lossy(&prefix),
                    String::from_utf8_lossy(it)
                );
                record.etag = b"_invalid_".to_vec();
            }
        }

        info!(
            target: LOG_TARGET,
            "Updating file record for path: {} inode: {} modtime: {} type: {:?} etag: {} fileId: {} fileSize: {} checksum: {}",
            record.path_str(),
            record.inode,
            record.modtime,
            record.item_type,
            String::from_utf8_lossy(&record.etag),
            String::from_utf8_lossy(&record.file_id),
            record.file_size,
            String::from_utf8_lossy(&record.checksum_header)
        );

        debug_assert!(!record.path.is_empty());

        let phash = get_phash(&record.path);
        if !inner.check_connect() {
            warn!(target: LOG_TARGET, "Failed to connect database.");
            return Err(JournalError::Connect);
        }

        let plen = record.path.len() as i64;
        let remote_perm = record.remote_perm.to_db_value();
        let (checksum_type, checksum) =
            parse_checksum_header(&record.checksum_header).unwrap_or_default();
        let content_checksum_type_id = inner.map_checksum_type(&checksum_type);

        let lock = &record.lockstate;
        let params: [&dyn ToSql; 34] = [
            &phash,
            &plen,
            &BaText(&record.path),
            &(record.inode as i64),
            &0, // uid Not used
            &0, // gid Not used
            &0, // mode Not used
            &record.modtime,
            &(record.item_type as i32),
            &BaText(&record.etag),
            &BaText(&record.file_id),
            &BaText(&remote_perm),
            &record.file_size,
            &i32::from(record.server_has_ignored_files),
            &BaText(&checksum),
            &content_checksum_type_id,
            &BaText(&record.e2e_mangled_name),
            &(record.e2e_encryption_status as i32),
            // `bindValue(19, {})` picks the QByteArray overload: an empty
            // QByteArray is bound as TEXT '' (sqlite3_bind_text with
            // constData() ""), not NULL. Found by the side-by-side bench.
            &BaText(b""),
            &i32::from(lock.locked),
            &lock.lock_owner_type,
            &QStr(&lock.lock_owner_display_name),
            &QStr(&lock.lock_owner_id),
            &QStr(&lock.lock_editor_app),
            &lock.lock_time,
            &lock.lock_timeout,
            &QStr(&lock.lock_token),
            &i32::from(record.is_shared),
            &record.last_share_state_fetched_timestamp,
            &i32::from(record.shared_by_me),
            &i32::from(record.is_live_photo),
            &QStr(&record.live_photo_file),
            &record.folder_quota.bytes_used,
            &record.folder_quota.bytes_available,
        ];
        let result = inner
            .conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO metadata \
                 (phash, pathlen, path, inode, uid, gid, mode, modtime, type, md5, fileid, remotePerm, filesize, ignoredChildrenRemote, \
                 contentChecksum, contentChecksumTypeId, e2eMangledName, isE2eEncrypted, e2eCertificateFingerprint, lock, lockType, lockOwnerDisplayName, lockOwnerId, \
                 lockOwnerEditor, lockTime, lockTimeout, lockToken, isShared, lastShareStateFetchedTimestmap, sharedByMe, isLivePhoto, livePhotoFile, quotaBytesUsed, quotaBytesAvailable) \
                 VALUES (?1 , ?2, ?3 , ?4 , ?5 , ?6 , ?7,  ?8 , ?9 , ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34);",
            )
            .and_then(|mut s| s.execute(&params[..]));
        if let Err(e) = result {
            warn!(target: LOG_TARGET, "database error: {e}");
            return Err(e.into());
        }

        // Can't be true anymore.
        inner.metadata_table_is_empty = false;
        Ok(())
    }

    /// Returns the record for `filename`, `None` if there is none.
    /// An empty filename finds nothing.
    pub fn get_file_record(&self, filename: &[u8]) -> Result<Option<SyncJournalFileRecord>> {
        let mut inner = self.lock();
        if inner.metadata_table_is_empty {
            return Ok(None); // no error, yet nothing found
        }
        if !inner.check_connect() {
            return Err(JournalError::Connect);
        }
        if filename.is_empty() {
            return Ok(None);
        }
        let result = inner
            .conn()
            .prepare_cached(concat!(get_file_record_query!(), " WHERE phash=?1"))
            .and_then(|mut s| {
                collect_rows(
                    &mut s,
                    [get_phash(filename)],
                    fill_file_record_from_get_query,
                )
            });
        match result {
            Ok(mut rows) => Ok(if rows.is_empty() {
                None
            } else {
                Some(rows.swap_remove(0))
            }),
            Err(e) => {
                warn!(target: LOG_TARGET, "No journal entry found for {} Error: {e}", String::from_utf8_lossy(filename));
                inner.close();
                Err(e.into())
            }
        }
    }

    pub fn get_file_record_by_e2e_mangled_name(
        &self,
        mangled_name: &str,
    ) -> Result<Option<SyncJournalFileRecord>> {
        let mut inner = self.lock();
        if inner.metadata_table_is_empty {
            return Ok(None);
        }
        if !inner.check_connect() {
            return Err(JournalError::Connect);
        }
        if mangled_name.is_empty() {
            return Ok(None);
        }
        let result = inner
            .conn()
            .prepare_cached(concat!(
                get_file_record_query!(),
                " WHERE e2eMangledName=?1"
            ))
            .and_then(|mut s| {
                collect_rows(
                    &mut s,
                    [QStr(mangled_name)],
                    fill_file_record_from_get_query,
                )
            });
        match result {
            Ok(mut rows) => Ok(if rows.is_empty() {
                None
            } else {
                Some(rows.swap_remove(0))
            }),
            Err(e) => {
                warn!(target: LOG_TARGET, "No journal entry found for mangled name {mangled_name} Error: {e}");
                inner.close();
                Err(e.into())
            }
        }
    }

    pub fn get_file_record_by_inode(&self, inode: u64) -> Result<Option<SyncJournalFileRecord>> {
        let mut inner = self.lock();
        if inode == 0 || inner.metadata_table_is_empty {
            return Ok(None);
        }
        if !inner.check_connect() {
            return Err(JournalError::Connect);
        }
        let mut rows = inner
            .conn()
            .prepare_cached(concat!(get_file_record_query!(), " WHERE inode=?1"))
            .and_then(|mut s| {
                collect_rows(&mut s, [inode as i64], fill_file_record_from_get_query)
            })?;
        Ok(if rows.is_empty() {
            None
        } else {
            Some(rows.swap_remove(0))
        })
    }

    pub fn get_file_records_by_file_id(
        &self,
        file_id: &[u8],
        mut row_callback: impl FnMut(&SyncJournalFileRecord),
    ) -> Result<()> {
        let rows = self.lock().get_file_records_by_file_id(file_id)?;
        rows.iter().for_each(&mut row_callback);
        Ok(())
    }

    /// All records below `path` (not including `path` itself), sorted so that
    /// the contents of a directory come directly after the directory.
    /// The empty path returns the whole tree.
    pub fn get_files_below_path(
        &self,
        path: &[u8],
        mut row_callback: impl FnMut(&SyncJournalFileRecord),
    ) -> Result<()> {
        let rows = {
            let mut inner = self.lock();
            if inner.metadata_table_is_empty {
                return Ok(()); // no error, yet nothing found
            }
            if !inner.check_connect() {
                return Err(JournalError::Connect);
            }
            if path.is_empty() {
                // Since the path column doesn't store the starting /, the getFilesBelowPathQuery
                // can't be used for the root path "". It would scan for (path > '/' and path < '0')
                // and find nothing. So, unfortunately, we have to use a different query for
                // retrieving the whole tree.
                let mut stmt = inner
                    .conn()
                    .prepare_cached(concat!(get_file_record_query!(), " ORDER BY path||'/' ASC"))?;
                collect_rows(&mut stmt, [], fill_file_record_from_get_query)?
            } else {
                // This query is used to skip discovery and fill the tree from the
                // database instead
                let mut stmt = inner.conn().prepare_cached(concat!(
                    get_file_record_query!(),
                    " WHERE ",
                    is_prefix_path_of!("?1", "path"),
                    " OR ",
                    is_prefix_path_of!("?1", "e2eMangledName"),
                    // We want to ensure that the contents of a directory are sorted
                    // directly behind the directory itself. Without this ORDER BY
                    // an ordering like foo, foo-2, foo/file would be returned.
                    // With the trailing /, we get foo-2, foo, foo/file. This property
                    // is used in fill_tree_from_db().
                    " ORDER BY path||'/' ASC"
                ))?;
                collect_rows(&mut stmt, [BaText(path)], fill_file_record_from_get_query)?
            }
        };
        rows.iter().for_each(&mut row_callback);
        Ok(())
    }

    /// The direct children of `path` (uses the `parent_hash` index).
    pub fn list_files_in_path(
        &self,
        path: &[u8],
        mut row_callback: impl FnMut(&SyncJournalFileRecord),
    ) -> Result<()> {
        let rows = {
            let mut inner = self.lock();
            if inner.metadata_table_is_empty {
                return Ok(());
            }
            if !inner.check_connect() {
                return Err(JournalError::Connect);
            }
            let mut stmt = inner.conn().prepare_cached(concat!(
                get_file_record_query!(),
                " WHERE parent_hash(path) = ?1 ORDER BY path||'/' ASC"
            ))?;
            collect_rows(
                &mut stmt,
                [get_phash(path)],
                fill_file_record_from_get_query,
            )?
        };
        for rec in &rows {
            let start = path.len() + 1;
            let has_slash_after = rec.path.len() > start
                && rec.path[start..].iter().position(|&c| c == b'/').is_some();
            if !rec.path.starts_with(path) || has_slash_after {
                warn!(
                    target: LOG_TARGET,
                    "hash collision {} {}",
                    String::from_utf8_lossy(path),
                    rec.path_str()
                );
                continue;
            }
            row_callback(rec);
        }
        Ok(())
    }

    /// Delete the record of `filename`, and with `recursively` everything below it.
    pub fn delete_file_record(&self, filename: &str, recursively: bool) -> Result<()> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            warn!(target: LOG_TARGET, "Failed to connect database.");
            return Err(JournalError::Connect);
        }
        // always delete the actual file.
        inner
            .conn()
            .prepare_cached("DELETE FROM metadata WHERE phash=?1")
            .and_then(|mut s| s.execute([get_phash(filename.as_bytes())]))
            .inspect_err(|e| warn!(target: LOG_TARGET, "database error: {e}"))?;
        if recursively {
            inner
                .conn()
                .prepare_cached(concat!(
                    "DELETE FROM metadata WHERE ",
                    is_prefix_path_of!("?1", "path")
                ))
                .and_then(|mut s| s.execute([QStr(filename)]))
                .inspect_err(|e| warn!(target: LOG_TARGET, "database error: {e}"))?;
        }
        Ok(())
    }

    pub fn update_file_record_checksum(
        &self,
        filename: &str,
        content_checksum: &[u8],
        content_checksum_type: &[u8],
    ) -> Result<()> {
        let mut inner = self.lock();
        info!(
            target: LOG_TARGET,
            "Updating file checksum {filename} {} {}",
            String::from_utf8_lossy(content_checksum),
            String::from_utf8_lossy(content_checksum_type)
        );
        let phash = get_phash(filename.as_bytes());
        if !inner.check_connect() {
            warn!(target: LOG_TARGET, "Failed to connect database.");
            return Err(JournalError::Connect);
        }
        let checksum_type_id = inner.map_checksum_type(content_checksum_type);
        inner
            .conn()
            .prepare_cached(
                "UPDATE metadata SET contentChecksum = ?2, contentChecksumTypeId = ?3 WHERE phash == ?1;",
            )
            .and_then(|mut s| {
                s.execute(rusqlite::params![phash, BaText(content_checksum), checksum_type_id])
            })
            .inspect_err(|e| warn!(target: LOG_TARGET, "database error: {e}"))?;
        Ok(())
    }

    pub fn update_local_metadata(
        &self,
        filename: &str,
        modtime: i64,
        size: i64,
        inode: u64,
        lock_info: &SyncJournalFileLockInfo,
    ) -> Result<()> {
        let mut inner = self.lock();
        info!(target: LOG_TARGET, "Updating local metadata for: {filename} {modtime} {size} {inode}");
        let phash = get_phash(filename.as_bytes());
        if !inner.check_connect() {
            warn!(target: LOG_TARGET, "Failed to connect database.");
            return Err(JournalError::Connect);
        }
        inner
            .conn()
            .prepare_cached(
                "UPDATE metadata \
                 SET inode=?2, modtime=?3, filesize=?4, lock=?5, lockType=?6, \
                 lockOwnerDisplayName=?7, lockOwnerId=?8, lockOwnerEditor = ?9, \
                 lockTime=?10, lockTimeout=?11, lockToken=?12 \
                 WHERE phash == ?1;",
            )
            .and_then(|mut s| {
                s.execute(rusqlite::params![
                    phash,
                    inode as i64,
                    modtime,
                    size,
                    i32::from(lock_info.locked),
                    lock_info.lock_owner_type,
                    QStr(&lock_info.lock_owner_display_name),
                    QStr(&lock_info.lock_owner_id),
                    QStr(&lock_info.lock_editor_app),
                    lock_info.lock_time,
                    lock_info.lock_timeout,
                    QStr(&lock_info.lock_token),
                ])
            })
            .inspect_err(|e| warn!(target: LOG_TARGET, "database error: {e}"))?;
        Ok(())
    }

    /// Whether at least one of the numeric `file_ids` is present in the
    /// journal (`fileid` cast to an integer, i.e. its numeric prefix).
    pub fn has_file_ids(&self, file_ids: &[i64]) -> bool {
        if file_ids.is_empty() {
            // no need to check the db if no file id matches
            return false;
        }
        let mut inner = self.lock();
        if !inner.check_connect() {
            return false;
        }
        // Strings are used here to avoid any surprises during serialisation.
        let param = format!(
            "[{}]",
            file_ids
                .iter()
                .map(|id| format!("\"{id}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let result = inner
            .conn()
            .prepare_cached(
                "SELECT 1 FROM metadata, json_each(?1) file_ids WHERE CAST(metadata.fileid AS INTEGER) = CAST(file_ids.value AS INTEGER) LIMIT 1;",
            )
            .and_then(|mut s| collect_rows(&mut s, [BaText(param.as_bytes())], |r| int_value(r, 0)));
        match result {
            Ok(rows) => rows.first() == Some(&1),
            Err(e) => {
                warn!(target: LOG_TARGET, "file id query failed: {e}");
                false
            }
        }
    }

    /// Returns whether the item or any subitems are (de)hydrated.
    pub fn has_hydrated_or_dehydrated_files(
        &self,
        filename: &[u8],
    ) -> Option<HasHydratedDehydrated> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return None;
        }
        let result = inner
            .conn()
            .prepare_cached(concat!(
                "SELECT DISTINCT type FROM metadata WHERE (",
                is_prefix_path_or_equal!("?1", "path"),
                " OR ?1 == '');"
            ))
            .and_then(|mut s| collect_rows(&mut s, [BaText(filename)], |r| int64_value(r, 0)));
        let types = match result {
            Ok(t) => t,
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                return None;
            }
        };
        let mut result = HasHydratedDehydrated::default();
        for t in types {
            match ItemType::from_db(t) {
                ItemType::File | ItemType::VirtualFileDehydration => result.has_hydrated = true,
                ItemType::VirtualFile | ItemType::VirtualFileDownload => {
                    result.has_dehydrated = true
                }
                _ => {}
            }
        }
        Some(result)
    }

    // ---- e2ee helpers (records only; E2EE itself is out of scope) --------

    pub fn get_root_e2e_folder_record(
        &self,
        remote_folder_path: &str,
    ) -> Result<Option<SyncJournalFileRecord>> {
        debug_assert!(!remote_folder_path.is_empty() && remote_folder_path != "/");
        if remote_folder_path.is_empty() || remote_folder_path == "/" {
            warn!(target: LOG_TARGET, "Invalid folder path!");
            return Err(JournalError::Connect);
        }
        let mut split: Vec<&str> = remote_folder_path
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if split.is_empty() {
            warn!(target: LOG_TARGET, "Invalid folder path!");
            return Err(JournalError::Connect);
        }
        let mut rec = None;
        while !split.is_empty() {
            rec = self.get_file_record(split.join("/").as_bytes())?;
            if let Some(r) = &rec
                && r.is_e2e_encrypted()
                && r.e2e_mangled_name.is_empty()
            {
                // it's a toplevel folder record
                return Ok(rec);
            }
            split.pop();
        }
        Ok(rec)
    }

    pub fn list_all_e2ee_folders_with_encryption_status_less_than(
        &self,
        status: i32,
        mut row_callback: impl FnMut(&SyncJournalFileRecord),
    ) -> Result<()> {
        let rows = {
            let mut inner = self.lock();
            if inner.metadata_table_is_empty {
                return Ok(());
            }
            if !inner.check_connect() {
                return Err(JournalError::Connect);
            }
            let mut stmt = inner.conn().prepare_cached(concat!(
                get_file_record_query!(),
                " WHERE type == 2 AND isE2eEncrypted >= ?1 AND isE2eEncrypted < ?2 ORDER BY path||'/' ASC"
            ))?;
            collect_rows(
                &mut stmt,
                [JournalDbEncryptionStatus::Encrypted as i32, status],
                fill_file_record_from_get_query,
            )?
        };
        rows.iter()
            .filter(|r| r.item_type != ItemType::Skip)
            .for_each(&mut row_callback);
        Ok(())
    }

    pub fn find_encrypted_ancestor_for_record(
        &self,
        filename: &str,
    ) -> Result<Option<SyncJournalFileRecord>> {
        let parent_path = match filename.rfind('/') {
            Some(i) => &filename[..i],
            None => "",
        };
        let mut components: Vec<&str> = parent_path.split('/').collect();
        let mut rec = None;
        while !components.is_empty() {
            let joined = components.join("/");
            rec = match self.get_file_record(joined.as_bytes()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(target: LOG_TARGET, "could not get file from local DB {joined}");
                    return Err(e);
                }
            };
            if let Some(r) = &rec
                && r.is_e2e_encrypted()
            {
                break;
            }
            components.pop();
        }
        Ok(rec)
    }

    // ---- key/value store -------------------------------------------------

    /// `keyValueStoreSet` with an integer value.
    pub fn key_value_store_set(&self, key: &str, value: i64) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("INSERT OR REPLACE INTO key_value_store (key, value) VALUES(?1, ?2);")
            .and_then(|mut s| s.execute(rusqlite::params![QStr(key), value]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    pub fn key_value_store_get_int(&self, key: &str, default_value: i64) -> i64 {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return default_value;
        }
        let result = inner
            .conn()
            .prepare_cached("SELECT value FROM key_value_store WHERE key=?1")
            .and_then(|mut s| collect_rows(&mut s, [QStr(key)], |r| int64_value(r, 0)));
        match result {
            Ok(rows) => rows.first().copied().unwrap_or(default_value),
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                default_value
            }
        }
    }

    pub fn key_value_store_delete(&self, key: &str) {
        let inner = self.lock();
        // Upstream does not call checkConnect() here.
        let Some(db) = &inner.db else {
            warn!(target: LOG_TARGET, "Failed to initOrReset _deleteKeyValueStoreQuery");
            return;
        };
        if let Err(e) = db
            .prepare_cached("DELETE FROM key_value_store WHERE key=?1;")
            .and_then(|mut s| s.execute([QStr(key)]))
        {
            warn!(target: LOG_TARGET, "Failed to exec _deleteKeyValueStoreQuery for key {key}: {e}");
        }
    }

    // ---- error blacklist -------------------------------------------------

    pub fn error_blacklist_entry(&self, file: &str) -> SyncJournalErrorBlacklistRecord {
        let mut entry = SyncJournalErrorBlacklistRecord::default();
        if file.is_empty() {
            return entry;
        }
        let mut inner = self.lock();
        if !inner.check_connect() {
            return entry;
        }
        let sql = Inner::blacklist_query_sql();
        let result = inner.conn().prepare_cached(&sql).and_then(|mut s| {
            collect_rows(&mut s, [QStr(file)], |r| SyncJournalErrorBlacklistRecord {
                last_try_etag: ba_value(r, 0),
                last_try_modtime: int64_value(r, 1),
                retry_count: int_value(r, 2),
                error_string: string_value(r, 3),
                last_try_time: int64_value(r, 4),
                ignore_duration: int64_value(r, 5),
                rename_target: string_value(r, 6),
                error_category: ErrorCategory::from_db(int64_value(r, 7)),
                request_id: ba_value(r, 8),
                file: file.to_owned(),
            })
        });
        match result {
            Ok(mut rows) if !rows.is_empty() => entry = rows.swap_remove(0),
            Ok(_) => {}
            Err(e) => warn!(target: LOG_TARGET, "database error: {e}"),
        }
        entry
    }

    pub fn set_error_blacklist_entry(&self, item: &SyncJournalErrorBlacklistRecord) {
        let mut inner = self.lock();
        info!(
            target: LOG_TARGET,
            "Setting blacklist entry for {} {} {} {} {} {} {} {} {:?}",
            item.file,
            item.retry_count,
            item.error_string,
            item.last_try_time,
            item.ignore_duration,
            item.last_try_modtime,
            String::from_utf8_lossy(&item.last_try_etag),
            item.rename_target,
            item.error_category
        );
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO blacklist \
                 (path, lastTryEtag, lastTryModtime, retrycount, errorstring, lastTryTime, ignoreDuration, renameTarget, errorCategory, requestId) \
                 VALUES ( ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .and_then(|mut s| {
                s.execute(rusqlite::params![
                    QStr(&item.file),
                    BaText(&item.last_try_etag),
                    item.last_try_modtime,
                    item.retry_count,
                    QStr(&item.error_string),
                    item.last_try_time,
                    item.ignore_duration,
                    QStr(&item.rename_target),
                    item.error_category as i32,
                    BaText(&item.request_id),
                ])
            })
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    pub fn wipe_error_blacklist_entry(&self, file: &str) {
        if file.is_empty() {
            return;
        }
        let mut inner = self.lock();
        if inner.check_connect()
            && let Err(e) = inner
                .conn()
                .execute("DELETE FROM blacklist WHERE path=?1", [QStr(file)])
        {
            inner.sql_fail("Deletion of blacklist item failed.", &e);
        }
    }

    pub fn wipe_error_blacklist_category(&self, category: ErrorCategory) {
        let mut inner = self.lock();
        if inner.check_connect()
            && let Err(e) = inner.conn().execute(
                "DELETE FROM blacklist WHERE errorCategory=?1",
                [category as i32],
            )
        {
            inner.sql_fail("Deletion of blacklist category failed.", &e);
        }
    }

    /// Deletes the whole blacklist; returns the number of deleted rows or -1.
    pub fn wipe_error_blacklist(&self) -> i64 {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return -1;
        }
        match inner.conn().execute("DELETE FROM blacklist", []) {
            Ok(n) => n as i64,
            Err(e) => {
                inner.sql_fail("Deletion of whole blacklist failed", &e);
                -1
            }
        }
    }

    pub fn error_black_list_entry_count(&self) -> i64 {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return 0;
        }
        match inner
            .conn()
            .query_row("SELECT count(*) FROM blacklist", [], |r| {
                Ok(int64_value(r, 0))
            }) {
            Ok(n) => n,
            Err(e) => {
                inner.sql_fail("Count number of blacklist entries failed", &e);
                0
            }
        }
    }

    pub fn delete_stale_error_blacklist_entries(&self, keep: &HashSet<String>) -> Result<()> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Err(JournalError::Connect);
        }
        let paths = {
            let mut stmt = inner.conn().prepare("SELECT path FROM blacklist")?;
            collect_rows(&mut stmt, [], |r| string_value(r, 0))?
        };
        let mut del = inner
            .conn()
            .prepare("DELETE FROM blacklist WHERE path = ?")?;
        for file in paths.iter().filter(|f| !keep.contains(*f)) {
            del.execute([QStr(file)])?;
        }
        Ok(())
    }

    /// Delete flags table entries that have no metadata correspondent.
    pub fn delete_stale_flags_entries(&self) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner.exec_sql(
            "DELETE FROM flags WHERE path != '' AND path NOT IN (SELECT path from metadata);",
        ) {
            inner.sql_fail("deleteStaleFlagsEntries", &e);
        }
    }

    // ---- download / upload info -----------------------------------------

    pub fn get_download_info(&self, file: &str) -> DownloadInfo {
        let mut inner = self.lock();
        let mut res = DownloadInfo::default();
        if inner.check_connect() {
            match inner
                .conn()
                .prepare_cached("SELECT tmpfile, etag, errorcount FROM downloadinfo WHERE path=?1")
                .and_then(|mut s| collect_rows(&mut s, [QStr(file)], to_download_info))
            {
                Ok(mut rows) if !rows.is_empty() => res = rows.swap_remove(0),
                Ok(_) => {}
                Err(e) => warn!(target: LOG_TARGET, "database error: {e}"),
            }
        }
        res
    }

    /// Stores `info`, or deletes the entry when `info.valid` is false.
    pub fn set_download_info(&self, file: &str, info: &DownloadInfo) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        let result = if info.valid {
            inner
                .conn()
                .prepare_cached(
                    "INSERT OR REPLACE INTO downloadinfo (path, tmpfile, etag, errorcount) VALUES ( ?1 , ?2, ?3, ?4 )",
                )
                .and_then(|mut s| {
                    s.execute(rusqlite::params![
                        QStr(file),
                        QStr(&info.tmpfile),
                        BaText(&info.etag),
                        info.error_count
                    ])
                })
        } else {
            inner
                .conn()
                .prepare_cached("DELETE FROM downloadinfo WHERE path=?1")
                .and_then(|mut s| s.execute([QStr(file)]))
        };
        if let Err(e) = result {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// Deletes the download infos whose path is not in `keep` and returns them.
    pub fn get_and_delete_stale_download_infos(&self, keep: &HashSet<String>) -> Vec<DownloadInfo> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        // The selected values *must* match the ones expected by to_download_info().
        let rows = match inner
            .conn()
            .prepare("SELECT tmpfile, etag, errorcount, path FROM downloadinfo")
            .and_then(|mut s| {
                collect_rows(&mut s, [], |r| (string_value(r, 3), to_download_info(r)))
            }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                return Vec::new();
            }
        };
        let mut deleted_entries = Vec::new();
        let mut superfluous = Vec::new();
        for (file, info) in rows {
            if !keep.contains(&file) {
                superfluous.push(file);
                deleted_entries.push(info);
            }
        }
        let result = inner
            .conn()
            .prepare_cached("DELETE FROM downloadinfo WHERE path=?1")
            .and_then(|mut s| {
                superfluous
                    .iter()
                    .try_for_each(|f| s.execute([QStr(f)]).map(drop))
            });
        if let Err(e) = result {
            warn!(target: LOG_TARGET, "database error: {e}");
            return Vec::new();
        }
        deleted_entries
    }

    pub fn download_info_count(&self) -> i64 {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return 0;
        }
        match inner
            .conn()
            .query_row("SELECT count(*) FROM downloadinfo", [], |r| {
                Ok(int64_value(r, 0))
            }) {
            Ok(n) => n,
            Err(e) => {
                inner.sql_fail("Count number of downloadinfo entries failed", &e);
                0
            }
        }
    }

    pub fn get_upload_info(&self, file: &str) -> UploadInfo {
        let mut inner = self.lock();
        let mut res = UploadInfo::default();
        if inner.check_connect() {
            match inner
                .conn()
                .prepare_cached(
                    "SELECT chunk, transferid, errorcount, size, modtime, contentChecksum FROM uploadinfo WHERE path=?1",
                )
                .and_then(|mut s| {
                    collect_rows(&mut s, [QStr(file)], |r| UploadInfo {
                        chunk_upload_v1: int_value(r, 0),
                        transferid: int64_value(r, 1) as u32,
                        error_count: int_value(r, 2),
                        size: int64_value(r, 3),
                        modtime: int64_value(r, 4),
                        content_checksum: ba_value(r, 5),
                        valid: true,
                    })
                }) {
                Ok(mut rows) if !rows.is_empty() => res = rows.swap_remove(0),
                Ok(_) => {}
                Err(e) => warn!(target: LOG_TARGET, "database error: {e}"),
            }
        }
        res
    }

    /// Stores `info`, or deletes the entry when `info.valid` is false.
    pub fn set_upload_info(&self, file: &str, info: &UploadInfo) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        let result = if info.valid {
            inner
                .conn()
                .prepare_cached(
                    "INSERT OR REPLACE INTO uploadinfo \
                     (path, chunk, transferid, errorcount, size, modtime, contentChecksum) \
                     VALUES ( ?1 , ?2, ?3 , ?4 ,  ?5, ?6 , ?7 )",
                )
                .and_then(|mut s| {
                    s.execute(rusqlite::params![
                        QStr(file),
                        info.chunk_upload_v1,
                        i64::from(info.transferid),
                        info.error_count,
                        info.size,
                        info.modtime,
                        BaText(&info.content_checksum)
                    ])
                })
        } else {
            inner
                .conn()
                .prepare_cached("DELETE FROM uploadinfo WHERE path=?1")
                .and_then(|mut s| s.execute([QStr(file)]))
        };
        if let Err(e) = result {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// Deletes the upload infos whose path is not in `keep`; returns the
    /// transfer ids that were removed.
    pub fn delete_stale_upload_infos(&self, keep: &HashSet<String>) -> Vec<u32> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        let Ok(rows) = inner
            .conn()
            .prepare("SELECT path,transferid FROM uploadinfo")
            .and_then(|mut s| {
                collect_rows(&mut s, [], |r| (string_value(r, 0), int_value(r, 1) as u32))
            })
        else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        let mut superfluous = Vec::new();
        for (file, id) in rows {
            if !keep.contains(&file) {
                superfluous.push(file);
                ids.push(id);
            }
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("DELETE FROM uploadinfo WHERE path=?1")
            .and_then(|mut s| {
                superfluous
                    .iter()
                    .try_for_each(|f| s.execute([QStr(f)]).map(drop))
            })
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
        ids
    }

    // ---- async poll ------------------------------------------------------

    pub fn get_poll_infos(&self) -> Vec<PollInfo> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        inner
            .conn()
            .prepare("SELECT path, modtime, filesize, pollpath FROM async_poll")
            .and_then(|mut s| {
                collect_rows(&mut s, [], |r| PollInfo {
                    file: string_value(r, 0),
                    modtime: int64_value(r, 1),
                    file_size: int64_value(r, 2),
                    url: string_value(r, 3),
                })
            })
            .unwrap_or_default()
    }

    /// Stores `info`, or deletes the entry for `info.file` when the url is empty.
    pub fn set_poll_info(&self, info: &PollInfo) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if info.url.is_empty() {
            debug!(target: LOG_TARGET, "Deleting Poll job {}", info.file);
            if let Err(e) = inner
                .conn()
                .execute("DELETE FROM async_poll WHERE path=?", [QStr(&info.file)])
            {
                inner.sql_fail("setPollInfo DELETE FROM async_poll", &e);
            }
        } else if let Err(e) = inner.conn().execute(
            "INSERT OR REPLACE INTO async_poll (path, modtime, filesize, pollpath) VALUES( ? , ? , ? , ? )",
            rusqlite::params![QStr(&info.file), info.modtime, info.file_size, QStr(&info.url)],
        ) {
            inner.sql_fail("setPollInfo INSERT OR REPLACE INTO async_poll", &e);
        }
    }

    // ---- selective sync --------------------------------------------------

    /// Returns the specified list from the database (entries end with `/`).
    pub fn get_selective_sync_list(&self, ty: SelectiveSyncListType) -> Result<Vec<String>> {
        self.lock().get_selective_sync_list(ty)
    }

    /// Writes the selective sync list (removes all other entries of that list).
    pub fn set_selective_sync_list(&self, ty: SelectiveSyncListType, list: &[String]) {
        self.lock().set_selective_sync_list(ty, list);
    }

    pub fn add_selective_sync_lists(&self, ty: SelectiveSyncListType, path: &str) -> Vec<String> {
        let path_with_trailing_slash = utility::trailing_slash_path(path);
        let mut inner = self.lock();
        let list = inner.get_selective_sync_list(ty).unwrap_or_default();
        let mut set: HashSet<String> = list.into_iter().collect();
        set.insert(path_with_trailing_slash);
        let mut black_list: Vec<String> = set.into_iter().collect();
        qstring_sort(&mut black_list);
        inner.set_selective_sync_list(ty, &black_list);
        info!(target: "nextcloud.sync.database.sql", "add {path} into {ty:?} {black_list:?}");
        black_list
    }

    pub fn remove_selective_sync_lists(
        &self,
        ty: SelectiveSyncListType,
        path: &str,
    ) -> Vec<String> {
        let path_with_trailing_slash = utility::trailing_slash_path(path);
        let mut inner = self.lock();
        let list = inner.get_selective_sync_list(ty).unwrap_or_default();
        let mut set: HashSet<String> = list.into_iter().collect();
        set.remove(&path_with_trailing_slash);
        let mut black_list: Vec<String> = set.into_iter().collect();
        qstring_sort(&mut black_list);
        inner.set_selective_sync_list(ty, &black_list);
        info!(target: "nextcloud.sync.database.sql", "remove {path} into {ty:?} {black_list:?}");
        black_list
    }

    // ---- discovery helpers -----------------------------------------------

    /// Forget file ids and inodes at and below `path` so the next sync does
    /// not detect renames there, and schedule it for remote discovery.
    pub fn avoid_renames_on_next_sync(&self, path: &[u8]) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner.conn().execute(
            concat!(
                "UPDATE metadata SET fileid = '', inode = '0' WHERE ",
                is_prefix_path_or_equal!("?1", "path")
            ),
            [BaText(path)],
        ) {
            inner.sql_fail(
                &format!(
                    "avoidRenamesOnNextSync path: {}",
                    String::from_utf8_lossy(path)
                ),
                &e,
            );
        }
        // We also need to remove the ETags so the update phase refreshes the directory paths
        // on the next sync
        inner.schedule_path_for_remote_discovery(path);
    }

    /// Make sure that on the next sync `file_name` and its parents are
    /// discovered from the server: their etags are set to `_invalid_`, and
    /// [`SyncJournalDb::set_file_record`] keeps them invalid until
    /// [`SyncJournalDb::clear_etag_storage_filter`] or [`SyncJournalDb::close`].
    pub fn schedule_path_for_remote_discovery(&self, file_name: &[u8]) {
        self.lock().schedule_path_for_remote_discovery(file_name);
    }

    /// Wipe the etag storage filter. Also done implicitly on close().
    pub fn clear_etag_storage_filter(&self) {
        self.lock().etag_storage_filter.clear();
    }

    /// Ensures full remote discovery happens on the next sync.
    pub fn force_remote_discovery_next_sync(&self) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        inner.force_remote_discovery_next_sync_locked();
    }

    // ---- checksum types / data fingerprint ------------------------------

    /// Returns the checksum type for an id.
    pub fn get_checksum_type(&self, checksum_type_id: i32) -> Vec<u8> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        match inner
            .conn()
            .prepare_cached("SELECT name FROM checksumtype WHERE id=?1")
            .and_then(|mut s| collect_rows(&mut s, [checksum_type_id], |r| ba_value(r, 0)))
        {
            Ok(mut rows) if !rows.is_empty() => rows.swap_remove(0),
            Ok(_) => {
                warn!(target: LOG_TARGET, "No checksum type mapping found for {checksum_type_id}");
                Vec::new()
            }
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                Vec::new()
            }
        }
    }

    /// The data fingerprint used to detect backups.
    pub fn data_fingerprint(&self) -> Vec<u8> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        inner
            .conn()
            .prepare_cached("SELECT fingerprint FROM datafingerprint")
            .and_then(|mut s| collect_rows(&mut s, [], |r| ba_value(r, 0)))
            .ok()
            .and_then(|mut rows| {
                if rows.is_empty() {
                    None
                } else {
                    Some(rows.swap_remove(0))
                }
            })
            .unwrap_or_default()
    }

    pub fn set_data_fingerprint(&self, data_fingerprint: &[u8]) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("DELETE FROM datafingerprint;")
            .and_then(|mut s| s.execute([]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("INSERT INTO datafingerprint (fingerprint) VALUES (?1);")
            .and_then(|mut s| s.execute([BaText(data_fingerprint)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    // ---- conflicts -------------------------------------------------------

    /// Store a new or updated conflict record.
    pub fn set_conflict_record(&self, record: &ConflictRecord) {
        self.set_conflict_record_in("conflicts", record);
    }

    /// Store a new or updated case clash conflict record.
    pub fn set_case_conflict_record(&self, record: &ConflictRecord) {
        self.set_conflict_record_in("caseconflicts", record);
    }

    fn set_conflict_record_in(&self, table: &str, record: &ConflictRecord) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        let sql = format!(
            "INSERT OR REPLACE INTO {table} (path, baseFileId, baseModtime, baseEtag, basePath) VALUES (?1, ?2, ?3, ?4, ?5);"
        );
        if let Err(e) = inner.conn().prepare_cached(&sql).and_then(|mut s| {
            s.execute(rusqlite::params![
                BaText(&record.path),
                BaText(&record.base_file_id),
                record.base_modtime,
                BaText(&record.base_etag),
                BaText(&record.initial_base_path)
            ])
        }) {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// Retrieve a conflict record by path of the file with the conflict tag.
    pub fn conflict_record(&self, path: &[u8]) -> ConflictRecord {
        let mut inner = self.lock();
        let mut entry = ConflictRecord::default();
        if !inner.check_connect() {
            return entry;
        }
        match inner
            .conn()
            .prepare_cached(
                "SELECT baseFileId, baseModtime, baseEtag, basePath FROM conflicts WHERE path=?1;",
            )
            .and_then(|mut s| {
                collect_rows(&mut s, [BaText(path)], |r| ConflictRecord {
                    path: path.to_vec(),
                    base_file_id: ba_value(r, 0),
                    base_modtime: int64_value(r, 1),
                    base_etag: ba_value(r, 2),
                    initial_base_path: ba_value(r, 3),
                })
            }) {
            Ok(mut rows) if !rows.is_empty() => entry = rows.swap_remove(0),
            Ok(_) => {}
            Err(e) => warn!(target: LOG_TARGET, "database error: {e}"),
        }
        entry
    }

    fn case_conflict_record_where(&self, column: &str, value: &str) -> ConflictRecord {
        let mut inner = self.lock();
        let mut entry = ConflictRecord::default();
        if !inner.check_connect() {
            return entry;
        }
        let sql = format!(
            "SELECT path, baseFileId, baseModtime, baseEtag, basePath FROM caseconflicts WHERE {column}=?1;"
        );
        match inner.conn().prepare_cached(&sql).and_then(|mut s| {
            collect_rows(&mut s, [QStr(value)], |r| ConflictRecord {
                path: ba_value(r, 0),
                base_file_id: ba_value(r, 1),
                base_modtime: int64_value(r, 2),
                base_etag: ba_value(r, 3),
                initial_base_path: ba_value(r, 4),
            })
        }) {
            Ok(mut rows) if !rows.is_empty() => entry = rows.swap_remove(0),
            Ok(_) => {}
            Err(e) => warn!(target: LOG_TARGET, "database error: {e}"),
        }
        entry
    }

    /// Retrieve a case clash conflict record by the path of its base file.
    pub fn case_conflict_record_by_base_path(&self, base_name_path: &str) -> ConflictRecord {
        self.case_conflict_record_where("basePath", base_name_path)
    }

    /// Retrieve a case clash conflict record by path of the file with the conflict tag.
    pub fn case_conflict_record_by_path(&self, path: &str) -> ConflictRecord {
        self.case_conflict_record_where("path", path)
    }

    /// Delete a case clash conflict record by path of the file with the conflict tag.
    pub fn delete_case_clash_conflict_by_path_record(&self, path: &str) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("DELETE FROM caseconflicts WHERE path=?1;")
            .and_then(|mut s| s.execute([QStr(path)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// All paths of case clash conflict files with records in the db.
    pub fn case_clash_conflict_record_paths(&self) -> Vec<Vec<u8>> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        inner
            .conn()
            .prepare_cached("SELECT path FROM caseconflicts;")
            .and_then(|mut s| collect_rows(&mut s, [], |r| ba_value(r, 0)))
            .unwrap_or_else(|e| {
                warn!(target: LOG_TARGET, "database error: {e}");
                Vec::new()
            })
    }

    /// Delete a conflict record by path of the file with the conflict tag.
    pub fn delete_conflict_record(&self, path: &[u8]) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("DELETE FROM conflicts WHERE path=?1;")
            .and_then(|mut s| s.execute([BaText(path)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// All paths of files with a conflict tag in the name and records in the db.
    pub fn conflict_record_paths(&self) -> Vec<Vec<u8>> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        inner
            .conn()
            .prepare("SELECT path FROM conflicts")
            .and_then(|mut s| collect_rows(&mut s, [], |r| ba_value(r, 0)))
            .unwrap_or_else(|e| {
                warn!(target: LOG_TARGET, "database error: {e}");
                Vec::new()
            })
    }

    /// Find the base name for a conflict file name, using the journal or the
    /// name pattern. Empty if it's not even a conflict file by pattern.
    pub fn conflict_file_base_name(&self, conflict_name: &[u8]) -> Vec<u8> {
        let conflict = self.conflict_record(conflict_name);
        let mut result = Vec::new();
        if conflict.is_valid()
            && self
                .get_file_records_by_file_id(&conflict.base_file_id, |record| {
                    if !record.path.is_empty() {
                        result = record.path.clone();
                    }
                })
                .is_err()
        {
            warn!(
                target: LOG_TARGET,
                "conflictFileBaseName failed to getFileRecordsByFileId: {}",
                String::from_utf8_lossy(conflict_name)
            );
        }
        if result.is_empty() {
            result = utility::conflict_file_base_name_from_pattern(conflict_name);
        }
        result
    }

    /// Delete every file entry; the next sync re-syncs everything as new.
    pub fn clear_file_table(&self) {
        let mut inner = self.lock();
        // Upstream does not call checkConnect() here.
        if inner.db.is_none() {
            return;
        }
        if let Err(e) = inner.exec_sql("DELETE FROM metadata;") {
            warn!(target: LOG_TARGET, "database error: {e}");
            inner.sql_fail("clearFileTable", &e);
        }
    }

    /// Set `VirtualFileDownload` on all `VirtualFile` entries below `path`
    /// (`""` marks everything) and invalidate the etags of the affected dirs.
    pub fn mark_virtual_file_for_download_recursively(&self, path: &[u8]) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner.conn().execute(
            concat!(
                "UPDATE metadata SET type=5 WHERE (",
                is_prefix_path_of!("?1", "path"),
                " OR ?1 == '') AND type=4;"
            ),
            [BaText(path)],
        ) {
            warn!(target: LOG_TARGET, "database error: {e}");
            inner.sql_fail(
                &format!(
                    "markVirtualFileForDownloadRecursively UPDATE metadata SET type=5 path: {}",
                    String::from_utf8_lossy(path)
                ),
                &e,
            );
        }
        if inner.db.is_none() {
            return;
        }
        // We also must make sure we do not read the files from the database
        // (same logic as in schedulePathForRemoteDiscovery).
        if let Err(e) = inner.conn().execute(
            concat!(
                "UPDATE metadata SET md5='_invalid_' WHERE (",
                is_prefix_path_of!("?1", "path"),
                " OR ?1 == '' OR ",
                is_prefix_path_or_equal!("path", "?1"),
                ") AND type == 2;"
            ),
            [BaText(path)],
        ) {
            warn!(target: LOG_TARGET, "database error: {e}");
            inner.sql_fail(
                &format!(
                    "markVirtualFileForDownloadRecursively UPDATE metadata SET md5='_invalid_' path: {}",
                    String::from_utf8_lossy(path)
                ),
                &e,
            );
        }
    }

    // ---- e2ee locked folders --------------------------------------------

    pub fn set_e2ee_locked_folder(&self, folder_id: &[u8], folder_token: &[u8]) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO e2EeLockedFolders (folderId, token) VALUES (?1, ?2);",
            )
            .and_then(|mut s| s.execute([BaText(folder_id), BaText(folder_token)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    pub fn e2ee_locked_folder(&self, folder_id: &[u8]) -> Vec<u8> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        inner
            .conn()
            .prepare_cached("SELECT token FROM e2EeLockedFolders WHERE folderId=?1;")
            .and_then(|mut s| collect_rows(&mut s, [BaText(folder_id)], |r| ba_value(r, 0)))
            .ok()
            .and_then(|mut rows| {
                if rows.is_empty() {
                    None
                } else {
                    Some(rows.swap_remove(0))
                }
            })
            .unwrap_or_default()
    }

    pub fn e2ee_locked_folders(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return Vec::new();
        }
        inner
            .conn()
            .prepare_cached("SELECT * FROM e2EeLockedFolders")
            .and_then(|mut s| collect_rows(&mut s, [], |r| (ba_value(r, 0), ba_value(r, 1))))
            .unwrap_or_default()
    }

    pub fn delete_e2ee_locked_folder(&self, folder_id: &[u8]) {
        let mut inner = self.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("DELETE FROM e2EeLockedFolders WHERE folderId=?1;")
            .and_then(|mut s| s.execute([BaText(folder_id)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    // ---- pin states --------------------------------------------------------

    /// Access to pin states stored in the database (`internalPinStates()`).
    pub fn internal_pin_states(&self) -> PinStateInterface<'_> {
        PinStateInterface { db: self }
    }
}

impl Drop for SyncJournalDb {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        if inner.db.is_some() {
            inner.close();
        }
    }
}

/// Grouping of all functions relating to pin states (`PinStateInterface`).
pub struct PinStateInterface<'a> {
    db: &'a SyncJournalDb,
}

impl PinStateInterface<'_> {
    /// The pin state for the path without considering parents; `Inherited`
    /// if there's no explicit state. `None` on db error.
    pub fn raw_for_path(&self, path: &[u8]) -> Option<PinState> {
        let mut inner = self.db.lock();
        if !inner.check_connect() {
            return None;
        }
        let rows = inner
            .conn()
            .prepare_cached("SELECT pinState FROM flags WHERE path == ?1;")
            .and_then(|mut s| collect_rows(&mut s, [BaText(path)], |r| int64_value(r, 0)));
        match rows {
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                None
            }
            // no-entry means Inherited
            Ok(rows) => Some(
                rows.first()
                    .map_or(PinState::Inherited, |&v| PinState::from_db(v)),
            ),
        }
    }

    /// The pin state for the path after inheriting from parents. Never
    /// `Inherited`: `AlwaysLocal` when nothing is set. `None` on db error.
    pub fn effective_for_path(&self, path: &[u8]) -> Option<PinState> {
        self.db.lock().effective_pin_for_path(path)
    }

    /// Like [`Self::effective_for_path`] but `Inherited` when some subitem
    /// has a different pin state.
    pub fn effective_for_path_recursive(&self, path: &[u8]) -> Option<PinState> {
        // Get the item's effective pin state. We'll compare subitem's pin states
        // against this.
        let base_pin = self.effective_for_path(path)?;

        let mut inner = self.db.lock();
        if !inner.check_connect() {
            return None;
        }
        // Find all the non-inherited pin states below the item
        let rows = inner
            .conn()
            .prepare_cached(concat!(
                "SELECT DISTINCT pinState FROM flags WHERE (",
                is_prefix_path_of!("?1", "path"),
                " OR ?1 == '') AND pinState is not null and pinState != 0;"
            ))
            .and_then(|mut s| collect_rows(&mut s, [BaText(path)], |r| int64_value(r, 0)));
        match rows {
            Err(e) => {
                warn!(target: LOG_TARGET, "database error: {e}");
                None
            }
            Ok(rows) => {
                // Check if they are all identical
                if rows.iter().any(|&v| PinState::from_db(v) != base_pin) {
                    Some(PinState::Inherited)
                } else {
                    Some(base_pin)
                }
            }
        }
    }

    /// Sets a path's pin state (no trailing slash; `""` is the root).
    pub fn set_for_path(&self, path: &[u8], state: PinState) {
        let mut inner = self.db.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached("INSERT OR REPLACE INTO flags(path, pinState) VALUES(?1, ?2);")
            .and_then(|mut s| s.execute(rusqlite::params![BaText(path), state as i32]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// Wipes pin states for a path and below (`""` wipes everything).
    pub fn wipe_for_path_and_below(&self, path: &[u8]) {
        let mut inner = self.db.lock();
        if !inner.check_connect() {
            return;
        }
        if let Err(e) = inner
            .conn()
            .prepare_cached(concat!(
                "DELETE FROM flags WHERE ",
                // Allow "" to delete everything
                " (",
                is_prefix_path_or_equal!("?1", "path"),
                " OR ?1 == '');"
            ))
            .and_then(|mut s| s.execute([BaText(path)]))
        {
            warn!(target: LOG_TARGET, "database error: {e}");
        }
    }

    /// All paths with their pin state as in the db (includes `""`).
    pub fn raw_list(&self) -> Option<Vec<(Vec<u8>, PinState)>> {
        let mut inner = self.db.lock();
        if !inner.check_connect() {
            return None;
        }
        match inner
            .conn()
            .prepare("SELECT path, pinState FROM flags;")
            .and_then(|mut s| {
                collect_rows(&mut s, [], |r| {
                    (ba_value(r, 0), PinState::from_db(int64_value(r, 1)))
                })
            }) {
            Ok(rows) => Some(rows),
            Err(e) => {
                warn!(target: LOG_TARGET, "SQL Error PinStateInterface::rawList {e}");
                inner.close();
                None
            }
        }
    }
}
