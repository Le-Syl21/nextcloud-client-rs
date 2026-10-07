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
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use nc_journal::csync::ItemType;

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

/// `_csync_vio_local_stat_mb`: `lstat` mapped to a [`FileStat`]. `None` when
/// `lstat` fails.
pub fn csync_vio_local_stat(path: &str, check_permissions_validity: bool) -> Option<FileStat> {
    let md = fs::symlink_metadata(path).ok()?;
    let ft = md.file_type();
    let item_type = if ft.is_dir() {
        ItemType::Directory
    } else if ft.is_file() {
        ItemType::File
    } else if ft.is_symlink() || std::os::unix::fs::FileTypeExt::is_socket(&ft) {
        ItemType::SoftLink
    } else {
        ItemType::Skip
    };
    let mut st = FileStat {
        item_type,
        inode: md.ino(),
        modtime: md.mtime(),
        size: md.size() as i64,
        ..FileStat::default()
    };
    if check_permissions_validity {
        st.is_permissions_invalid = md.mode() & 0o002 == 0o002;
    }
    Some(st)
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

/// `csync_vio_local_opendir` + `readdir` loop: the entries of a directory
/// (without `.` and `..`), each stat'ed like `csync_vio_local_readdir`.
/// Entries whose stat fails get [`ItemType::Skip`].
pub fn csync_vio_local_readdir(
    path: &str,
    check_permissions_validity: bool,
) -> Result<Vec<FileStat>, OpendirError> {
    let rd = fs::read_dir(path).map_err(|e| match e.raw_os_error() {
        Some(13) => OpendirError::AccessDenied,
        Some(2) => OpendirError::NotFound,
        Some(20) => OpendirError::NotADirectory,
        _ => OpendirError::Other,
    })?;
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|_| OpendirError::Other)?;
        let name = std::os::unix::ffi::OsStrExt::as_bytes(entry.file_name().as_os_str()).to_vec();
        let full = format!(
            "{}/{}",
            path.trim_end_matches('/'),
            String::from_utf8_lossy(&name)
        );
        // d_type first, then lstat (which overrides it).
        let mut st = match entry.file_type() {
            Ok(ft) if ft.is_dir() => FileStat {
                item_type: ItemType::Directory,
                ..FileStat::default()
            },
            Ok(ft) if ft.is_file() => FileStat {
                item_type: ItemType::File,
                ..FileStat::default()
            },
            _ => FileStat::default(),
        };
        if std::str::from_utf8(&name).is_ok() {
            match csync_vio_local_stat(&full, check_permissions_validity) {
                Some(s) => st = s,
                // Will get excluded by _csync_detect_update.
                None => st.item_type = ItemType::Skip,
            }
        }
        st.name = name;
        out.push(st);
    }
    Ok(out)
}

/// `FileSystem::fileExists` (follows symlinks, like `QFileInfo::exists`).
pub fn file_exists(path: &str) -> bool {
    Path::new(path).exists()
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

/// `FileSystem::isWritable` (`QFileInfo::isWritable`).
pub fn is_writable(path: &str) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
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
            let t = fs::metadata(path).map(|m| m.mtime()).unwrap_or(-1);
            log::warn!(target: "nextcloud.sync.filesystem", "Could not get modification time for {path} with csync, using QFileInfo: {t}");
            t
        }
    }
}

/// `FileSystem::setModTime` (`c_utimes`: access and modification time).
pub fn set_mod_time(path: &str, mod_time: i64) -> bool {
    let ts = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: mod_time,
            tv_nsec: 0,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: mod_time,
            tv_nsec: 0,
        },
    };
    match rustix::fs::utimensat(rustix::fs::CWD, path, &ts, rustix::fs::AtFlags::empty()) {
        Ok(()) => true,
        Err(e) => {
            log::warn!(target: "nextcloud.sync.filesystem", "Error setting mtime for {path} failed: {e}");
            false
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

/// `QFile::rename`: fails when the destination exists.
pub fn rename(origin: &str, destination: &str) -> Result<(), String> {
    if fs::symlink_metadata(destination).is_ok() {
        let e = "Destination file exists".to_owned();
        log::warn!(target: "nextcloud.sync.filesystem", "Error renaming file {origin} to {destination} failed: {e}");
        return Err(e);
    }
    fs::rename(origin, destination).map_err(|e| {
        log::warn!(target: "nextcloud.sync.filesystem", "Error renaming file {origin} to {destination} failed: {e}");
        e.to_string()
    })
}

/// `FileSystem::uncheckedRenameReplace`: rename, overwriting the destination.
pub fn unchecked_rename_replace(origin: &str, destination: &str) -> Result<(), String> {
    if file_exists(destination) && fs::remove_file(destination).is_err() {
        log::warn!(target: "nextcloud.sync.filesystem", "Target file could not be removed.");
        return Err("Target file could not be removed".to_owned());
    }
    fs::rename(origin, destination).map_err(|e| {
        log::warn!(target: "nextcloud.sync.filesystem", "Renaming temp file to final failed: {e}");
        e.to_string()
    })
}

/// `FileSystem::remove` (a file).
pub fn remove(path: &str) -> Result<(), String> {
    if fs::symlink_metadata(path).is_err() {
        log::info!(target: "nextcloud.sync.filesystem", "{path} has been already deleted");
        return Ok(());
    }
    fs::remove_file(path).map_err(|e| {
        log::warn!(target: "nextcloud.sync.filesystem", "{e} {path}");
        e.to_string()
    })
}

/// The write permissions `setFileReadOnly(false)` adds (`getDefaultWritePermissions`).
fn default_write_permissions() -> u32 {
    use std::sync::OnceLock;
    static PERMS: OnceLock<u32> = OnceLock::new();
    *PERMS.get_or_init(|| {
        let mask = rustix::process::umask(rustix::fs::Mode::empty());
        rustix::process::umask(mask);
        let mask = mask.bits();
        let mut result = 0o200;
        if mask & 0o020 == 0 {
            result |= 0o020;
        }
        if mask & 0o002 == 0 {
            result |= 0o002;
        }
        result
    })
}

fn mode(path: &str) -> Option<u32> {
    fs::metadata(path).ok().map(|m| m.permissions().mode())
}

/// `FileSystem::setFileReadOnly`.
pub fn set_file_read_only(path: &str, readonly: bool) {
    let Some(m) = mode(path) else {
        return;
    };
    let mut m = m & !0o222;
    if !readonly {
        m |= default_write_permissions();
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(m));
}

/// `FileSystem::setFileReadOnlyWeak`: only makes writable when the owner
/// cannot write. Returns whether permissions were changed.
pub fn set_file_read_only_weak(path: &str, readonly: bool) -> bool {
    let Some(m) = mode(path) else {
        return false;
    };
    if !readonly && m & 0o200 != 0 {
        return false; // already writable enough
    }
    set_file_read_only(path, readonly);
    true
}

/// `FileSystem::FolderPermissions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderPermissions {
    ReadOnly,
    ReadWrite,
}

/// `FileSystem::setFolderPermissions` (Unix).
pub fn set_folder_permissions(path: &str, permissions: FolderPermissions) -> bool {
    let Some(current) = mode(path) else {
        log::warn!(target: "nextcloud.sync.filesystem", "exception when modifying folder permissions - path: {path}");
        return false;
    };
    let new = match permissions {
        FolderPermissions::ReadOnly => current & !0o222,
        FolderPermissions::ReadWrite => (current & !0o002) | 0o200,
    };
    if new != current && fs::set_permissions(path, fs::Permissions::from_mode(new)).is_err() {
        return false;
    }
    true
}

/// `FileSystem::isFolderReadOnly` (Unix): the owner cannot write.
pub fn is_folder_read_only(path: &str) -> bool {
    mode(path).is_some_and(|m| m & 0o200 != 0o200)
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
        Some(i) => path[..i].to_owned(),
        None => ".".to_owned(),
    }
}

/// `FileSystem::isFileLocked`: only Windows has mandatory locks.
pub fn is_file_locked(_path: &str) -> bool {
    false
}

/// `FileSystem::setFileHidden`: a no-op outside Windows.
pub fn set_file_hidden(_path: &str, _hidden: bool) {}

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

/// `Utility::freeDiskSpace`: available bytes, -1 if unknown.
pub fn free_disk_space(path: &str) -> i64 {
    match rustix::fs::statvfs(path) {
        Ok(s) => (s.f_bavail as i64).saturating_mul(s.f_frsize as i64),
        Err(_) => -1,
    }
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
        std::os::unix::fs::symlink(&p, &link).unwrap();
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

    #[test]
    fn derived_read_only_helpers() {
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
