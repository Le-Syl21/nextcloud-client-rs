// SPDX-FileCopyrightText: 2015 ownCloud GmbH
// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the Unix parts of upstream `src/libsync/filesystem.cpp`,
// `src/common/filesystembase.cpp` (LGPL-2.1-or-later upstream, used here as
// part of a GPL-2.0-or-later work) and `src/csync/vio/csync_vio_local_unix.cpp`
// (nextcloud/desktop v34.0.5).

//! File system helpers (`OCC::FileSystem`) and the local stat/readdir of
//! csync (`csync_vio_local_*`). Paths are `/`-separated strings, like
//! upstream's `QString` paths.

use std::fs;
use std::io::Read;
use std::path::Path;

use nc_journal::csync::ItemType;

pub mod lock_file;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

pub use sys::{
    csync_vio_local_readdir, csync_vio_local_stat, free_disk_space, is_file_locked_mode,
    is_folder_read_only, is_writable, set_file_hidden, set_file_read_only, set_file_read_only_weak,
    set_folder_permissions_changed, set_mod_time,
};

pub use lock_file::{
    FileLockingInfo, FileLockingType, file_path_lock_file_pattern_match,
    find_all_lock_files_in_dir, is_matching_adobe_document_extension,
    is_matching_affinity_document_extension, is_matching_autocad_document_extension,
    is_matching_office_file_extension, lock_file_target_file_path,
};

/// `csync_file_stat_t`, the fields the sync engine reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileStat {
    /// The entry name (readdir) as raw bytes.
    pub name: Vec<u8>,
    pub item_type: ItemType,
    pub inode: u64,
    pub modtime: i64,
    pub size: i64,
    pub is_hidden: bool,
    pub is_permissions_invalid: bool,
    pub is_metadata_missing: bool,
}

/// Why opening a directory for listing failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpendirError {
    /// `EACCES`
    AccessDenied,
    /// `ENOENT`
    NotFound,
    /// `ENOTDIR`
    NotADirectory,
    /// Anything else.
    Other,
}

/// `FileSystem::fileExists` (follows symlinks, like `QFileInfo::exists`).
pub fn file_exists(path: &str) -> bool {
    Path::new(&*native(path)).exists()
}

/// `FileSystem::isDir`.
pub fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// `FileSystem::isFile`.
pub fn is_file(path: &str) -> bool {
    Path::new(path).is_file()
}

/// `FileSystem::isSymLink`.
pub fn is_sym_link(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// `FileSystem::isLnkFile`.
pub fn is_lnk_file(filename: &str) -> bool {
    filename.ends_with(".lnk")
}

/// `FileSystem::isExcludeFile`.
pub fn is_exclude_file(filename: &str) -> bool {
    let l = filename.to_lowercase();
    l == ".sync-exclude.lst"
        || l == "exclude.lst"
        || l.ends_with("/.sync-exclude.lst")
        || l.ends_with("/exclude.lst")
}

/// `FileSystem::getModTime`: the lstat mtime, falling back to the followed
/// `stat` mtime (`QFileInfo::lastModified`). -1 when neither works.
pub fn get_mod_time(path: &str) -> i64 {
    match csync_vio_local_stat(path, true) {
        Some(st) if st.modtime != 0 => st.modtime,
        _ => {
            let t = fs::metadata(path)
                .map(|m| sys::metadata_mtime(&m))
                .unwrap_or(-1);
            log::warn!(target: "nextcloud.sync.filesystem", "Could not get modification time for {path} with csync, using QFileInfo: {t}");
            t
        }
    }
}

/// `FileSystem::getSize` (`QFileInfo::size`: follows symlinks, 0 on error).
pub fn get_size(path: &str) -> i64 {
    fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0)
}

/// `FileSystem::getInode`.
pub fn get_inode(path: &str) -> Option<u64> {
    csync_vio_local_stat(path, true).map(|s| s.inode)
}

/// `FileSystem::fileChanged`.
pub fn file_changed(path: &str, previous_size: i64, previous_mtime: i64) -> bool {
    get_size(path) != previous_size || get_mod_time(path) != previous_mtime
}

/// `FileSystem::verifyFileUnchanged`.
pub fn verify_file_unchanged(path: &str, previous_size: i64, previous_mtime: i64) -> bool {
    let actual_size = get_size(path);
    let actual_mtime = get_mod_time(path);
    if (actual_size != previous_size && actual_mtime > 0)
        || (actual_mtime != previous_mtime && previous_mtime > 0 && actual_mtime > 0)
    {
        log::info!(target: "nextcloud.sync.filesystem", "File {path} has changed: size: {previous_size} <-> {actual_size}, mtime: {previous_mtime} <-> {actual_mtime}");
        return false;
    }
    true
}

/// The form of a path handed to the Win32 calls that must not normalize it
/// (`FileSystem::longWinPath`): verbatim on Windows, unchanged elsewhere.
fn native(path: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) {
        std::borrow::Cow::Owned(nc_platform::paths::long_win_path(path))
    } else {
        std::borrow::Cow::Borrowed(path)
    }
}

/// `QFile::rename`: fails when the destination exists.
pub fn rename(origin: &str, destination: &str) -> Result<(), String> {
    // On a case-insensitive file system a rename that only changes the case
    // finds the destination: it is the file itself.
    let same_file_other_case = cfg!(any(windows, target_os = "macos"))
        && origin.to_lowercase() == destination.to_lowercase();
    if !same_file_other_case && fs::symlink_metadata(&*native(destination)).is_ok() {
        let e = "Destination file exists".to_owned();
        log::warn!(target: "nextcloud.sync.filesystem", "Error renaming file {origin} to {destination} failed: {e}");
        return Err(e);
    }
    fs::rename(&*native(origin), &*native(destination)).map_err(|e| {
        log::warn!(target: "nextcloud.sync.filesystem", "Error renaming file {origin} to {destination} failed: {e}");
        e.to_string()
    })
}

/// `FileSystem::uncheckedRenameReplace`: rename, overwriting the destination.
pub fn unchecked_rename_replace(origin: &str, destination: &str) -> Result<(), String> {
    if file_exists(destination) && fs::remove_file(&*native(destination)).is_err() {
        log::warn!(target: "nextcloud.sync.filesystem", "Target file could not be removed.");
        return Err("Target file could not be removed".to_owned());
    }
    fs::rename(&*native(origin), &*native(destination)).map_err(|e| {
        log::warn!(target: "nextcloud.sync.filesystem", "Renaming temp file to final failed: {e}");
        e.to_string()
    })
}

/// `FileSystem::remove` (a file).
pub fn remove(path: &str) -> Result<(), String> {
    if fs::symlink_metadata(&*native(path)).is_err() {
        log::info!(target: "nextcloud.sync.filesystem", "{path} has been already deleted");
        return Ok(());
    }
    fs::remove_file(&*native(path)).map_err(|e| {
        log::warn!(target: "nextcloud.sync.filesystem", "{e} {path}");
        e.to_string()
    })
}

/// `FileSystem::LockMode`: how `isFileLocked` probes (the share mode of
/// the probing handle on Windows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockMode {
    /// Nobody else may have the file open.
    #[default]
    Exclusive,
    /// Others may read and write.
    Shared,
    /// Others may read and write, the probe only reads.
    SharedRead,
}

/// `FileSystem::isFileLocked(fileName)` (exclusive mode): only Windows has
/// mandatory locks.
pub fn is_file_locked(path: &str) -> bool {
    is_file_locked_mode(path, LockMode::Exclusive)
}

/// `FileSystem::FolderPermissions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderPermissions {
    ReadOnly,
    ReadWrite,
}

/// `FileSystem::setFolderPermissions` (Unix).
pub fn set_folder_permissions(path: &str, permissions: FolderPermissions) -> bool {
    set_folder_permissions_changed(path, permissions, &mut false)
}

/// `FileSystem::FilePermissionsRestore`: temporarily gives a folder the
/// requested permissions, restored on drop.
pub struct FilePermissionsRestore {
    path: String,
    initial: FolderPermissions,
    rollback_needed: bool,
}

impl FilePermissionsRestore {
    pub fn new(path: &str, temporary: FolderPermissions) -> Self {
        let initial = if is_folder_read_only(path) {
            FolderPermissions::ReadOnly
        } else {
            FolderPermissions::ReadWrite
        };
        let mut rollback_needed = false;
        if initial != temporary {
            set_folder_permissions(path, temporary);
            rollback_needed = true;
        }
        Self {
            path: path.to_owned(),
            initial,
            rollback_needed,
        }
    }
}

impl Drop for FilePermissionsRestore {
    fn drop(&mut self) {
        if self.rollback_needed {
            set_folder_permissions(&self.path, self.initial);
        }
    }
}

/// The parent directory of a `/`-separated path (`QFileInfo::dir().absolutePath()`).
pub fn parent_dir(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) => "/".to_owned(),
        // "C:/name": the parent is the drive root
        Some(2) if cfg!(windows) && path.as_bytes()[1] == b':' => path[..3].to_owned(),
        Some(i) => path[..i].to_owned(),
        None => ".".to_owned(),
    }
}

/// `FileSystem::fileEquals`: same content.
pub fn file_equals(a: &str, b: &str) -> bool {
    let (Ok(mut f1), Ok(mut f2)) = (fs::File::open(a), fs::File::open(b)) else {
        log::warn!(target: "nextcloud.sync.filesystem", "fileEquals: Failed to open {a} or {b}");
        return false;
    };
    if get_size(a) != get_size(b) {
        return false;
    }
    let mut b1 = vec![0u8; 16 * 1024];
    let mut b2 = vec![0u8; 16 * 1024];
    loop {
        let n1 = f1.read(&mut b1).unwrap_or(0);
        let n2 = f2.read(&mut b2).unwrap_or(0);
        if n1 != n2 || b1[..n1] != b2[..n2] {
            return false;
        }
        if n1 == 0 {
            return true;
        }
    }
}

/// `FileSystem::removeRecursively`: deletes a directory tree. `on_deleted`
/// is called for every removed entry (path, is_dir); `on_error` for every
/// failure. Returns whether everything was removed.
pub fn remove_recursively(
    path: &str,
    on_deleted: &mut dyn FnMut(&str, bool),
    errors: &mut Vec<String>,
) -> bool {
    if !set_folder_permissions(path, FolderPermissions::ReadWrite) {
        errors.push(format!("Could not change permissions of \"{path}\""));
    }
    let mut all_removed = true;
    let entries = match fs::read_dir(path) {
        Ok(rd) => rd.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    for entry in entries {
        let child = format!("{}/{}", path, entry.file_name().to_string_lossy());
        let is_directory = is_dir(&child) && !is_sym_link(&child);
        let remove_ok = if is_directory {
            remove_recursively(&child, on_deleted, errors)
        } else {
            let _restore = FilePermissionsRestore::new(path, FolderPermissions::ReadWrite);
            match remove(&child) {
                Ok(()) => {
                    on_deleted(&child, false);
                    true
                }
                Err(e) => {
                    errors.push(format!("Error removing \"{child}\": {e}"));
                    log::warn!(target: "nextcloud.sync.filesystem", "Error removing {child}: {e}");
                    false
                }
            }
        };
        if !remove_ok {
            all_removed = false;
        }
    }
    if all_removed {
        let _restore = FilePermissionsRestore::new(&parent_dir(path), FolderPermissions::ReadWrite);
        match fs::remove_dir(path) {
            Ok(()) => on_deleted(path, true),
            Err(e) => {
                errors.push(format!("Could not remove folder \"{path}\""));
                log::warn!(target: "nextcloud.sync.filesystem", "Error removing folder {path} {e}");
                all_removed = false;
            }
        }
    }
    all_removed
}

/// `QDir::mkpath`.
pub fn mkpath(path: &str) -> bool {
    fs::create_dir_all(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_stat_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = format!("{}/f", dir.path().display());
        fs::write(&p, b"hello").unwrap();
        assert!(set_mod_time(&p, 1_000_000));
        assert_eq!(get_mod_time(&p), 1_000_000);
        let st = csync_vio_local_stat(&p, true).unwrap();
        assert_eq!(st.item_type, ItemType::File);
        assert_eq!(st.size, 5);
        assert!(verify_file_unchanged(&p, 5, 1_000_000));
        assert!(!verify_file_unchanged(&p, 6, 1_000_000));
        let link = format!("{}/l", dir.path().display());
        // Creating a symlink needs a privilege on Windows.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&p, &link).unwrap();
        #[cfg(not(unix))]
        fs::write(&link, b"x").unwrap();
        #[cfg(unix)]
        assert_eq!(
            csync_vio_local_stat(&link, true).unwrap().item_type,
            ItemType::SoftLink
        );
        let list = csync_vio_local_readdir(&dir.path().display().to_string(), true).unwrap();
        assert_eq!(list.len(), 2);
        assert!(rename(&p, &link).is_err());
        assert!(unchecked_rename_replace(&p, &link).is_ok());
        assert_eq!(parent_dir("/a/b/c"), "/a/b");
        assert_eq!(parent_dir("/a"), "/");
    }

    // The folder permissions of Windows are ACLs, not ported yet.
    #[cfg(unix)]
    #[test]
    fn derived_read_only_helpers() {
        use super::unix::mode;
        let dir = tempfile::tempdir().unwrap();
        let d = format!("{}/d", dir.path().display());
        fs::create_dir(&d).unwrap();
        assert!(!is_folder_read_only(&d));
        assert!(set_folder_permissions(&d, FolderPermissions::ReadOnly));
        assert!(is_folder_read_only(&d));
        {
            let _r = FilePermissionsRestore::new(&d, FolderPermissions::ReadWrite);
            assert!(!is_folder_read_only(&d));
        }
        assert!(is_folder_read_only(&d));
        set_folder_permissions(&d, FolderPermissions::ReadWrite);
        let f = format!("{d}/f");
        fs::write(&f, b"x").unwrap();
        set_file_read_only(&f, true);
        assert_eq!(mode(&f).unwrap() & 0o222, 0);
        assert!(set_file_read_only_weak(&f, false));
        assert!(!set_file_read_only_weak(&f, false));
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        assert!(remove_recursively(
            &d,
            &mut |p, isdir| deleted.push((p.to_owned(), isdir)),
            &mut errors
        ));
        assert_eq!(deleted.len(), 2);
        assert!(!file_exists(&d));
    }
}
