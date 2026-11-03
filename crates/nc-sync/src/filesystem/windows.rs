// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The Windows side of the local file system layer: ports of
//! `csync_vio_local_win.cpp` and the `Q_OS_WIN` parts of
//! `filesystembase.cpp`. Paths stay `/`-separated strings; the Win32 calls
//! that must not normalize a path (trailing dots and spaces, long paths) get
//! the verbatim form of [`nc_platform::paths::long_win_path`].

use std::fs;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

use nc_journal::csync::ItemType;
use nc_platform::paths::long_win_path;

use super::{FileStat, LockMode, OpendirError};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_DEVICE: u32 = 0x40;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;

/// `FileTimeToUnixTime`: 100 ns ticks since 1601 to seconds since 1970.
fn file_time_to_unix(ft: u64) -> i64 {
    (ft / 10_000_000) as i64 - 11_644_473_600
}

/// The modification time of a followed `stat`.
pub(super) fn metadata_mtime(md: &fs::Metadata) -> i64 {
    file_time_to_unix(md.last_write_time())
}

/// `_csync_vio_local_stat_mb`: the entry itself (reparse points are not
/// followed), the inode is the file index.
pub fn csync_vio_local_stat(path: &str, _check_permissions_validity: bool) -> Option<FileStat> {
    let md = fs::symlink_metadata(long_win_path(path)).ok()?;
    let attrs = md.file_attributes();
    let ft = md.file_type();
    let item_type = if attrs & (FILE_ATTRIBUTE_DEVICE | FILE_ATTRIBUTE_OFFLINE) != 0 {
        ItemType::Skip
    } else if ft.is_symlink() {
        // symlinks and junctions
        ItemType::SoftLink
    } else if ft.is_dir() {
        ItemType::Directory
    } else if ft.is_file() {
        ItemType::File
    } else {
        ItemType::Skip
    };
    let inode = nc_platform::windows::file_info(path).map_or(0, |i| i.file_index);
    Some(FileStat {
        item_type,
        inode,
        modtime: metadata_mtime(&md),
        size: md.len() as i64,
        is_hidden: attrs & FILE_ATTRIBUTE_HIDDEN != 0,
        ..FileStat::default()
    })
}

/// `csync_vio_local_opendir` + `readdir` loop (`FindFirstFile` upstream).
/// Names that are not valid Unicode (unpaired surrogates) get
/// [`ItemType::Skip`].
pub fn csync_vio_local_readdir(
    path: &str,
    check_permissions_validity: bool,
) -> Result<Vec<FileStat>, OpendirError> {
    let rd = fs::read_dir(long_win_path(path)).map_err(|e| match e.raw_os_error() {
        Some(5) => OpendirError::AccessDenied,
        Some(2 | 3) => OpendirError::NotFound,
        Some(267) => OpendirError::NotADirectory,
        _ => OpendirError::Other,
    })?;
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|_| OpendirError::Other)?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            out.push(FileStat {
                name: file_name.to_string_lossy().into_owned().into_bytes(),
                item_type: ItemType::Skip,
                ..FileStat::default()
            });
            continue;
        };
        let full = format!("{}/{}", path.trim_end_matches(['/', '\\']), name);
        let mut st = match csync_vio_local_stat(&full, check_permissions_validity) {
            Some(s) => s,
            // Will get excluded by _csync_detect_update.
            None => FileStat {
                item_type: ItemType::Skip,
                ..FileStat::default()
            },
        };
        st.name = name.as_bytes().to_vec();
        out.push(st);
    }
    Ok(out)
}

/// `FileSystem::isWritable`: a directory is, a file when it is not
/// read-only. (Upstream also looks at the NTFS permissions.)
pub fn is_writable(path: &str) -> bool {
    match fs::metadata(long_win_path(path)) {
        Ok(md) => md.is_dir() || md.file_attributes() & FILE_ATTRIBUTE_READONLY == 0,
        Err(_) => false,
    }
}

/// `FileSystem::setModTime` (`SetFileTime` with `BACKUP_SEMANTICS`, access
/// and modification time). Like upstream, a read-only file is made writable
/// for the call.
pub fn set_mod_time(path: &str, mod_time: i64) -> bool {
    let native = long_win_path(path);
    let was_readonly = fs::metadata(&native).is_ok_and(|m| m.permissions().readonly());
    if was_readonly {
        set_file_read_only(path, false);
    }
    let result = nc_platform::windows::set_file_times(path, mod_time);
    if was_readonly {
        set_file_read_only(path, true);
    }
    match result {
        Ok(()) => true,
        Err(e) => {
            log::warn!(target: "nextcloud.sync.filesystem", "Error setting mtime for {path} failed: {e}");
            false
        }
    }
}

/// `FileSystem::setFileReadOnly`: the read-only attribute, and the ACL
/// that makes the object read-only (read-only: replace any existing deny ACE
/// with one that still lets us change the attributes; read-write: remove the
/// deny ACEs, they could block clearing the attribute).
pub fn set_file_read_only(path: &str, readonly: bool) {
    let native = long_win_path(path);
    let Ok(md) = fs::metadata(&native) else {
        return;
    };
    nc_platform::windows::set_acl_permission(path, readonly);
    let mut perms = md.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(readonly);
    let _ = fs::set_permissions(&native, perms);
}

/// `FileSystem::setFileReadOnlyWeak`: only makes writable when the file is
/// read-only. Returns whether the attribute was changed.
pub fn set_file_read_only_weak(path: &str, readonly: bool) -> bool {
    let Ok(md) = fs::metadata(long_win_path(path)) else {
        return false;
    };
    if !readonly && !md.permissions().readonly() {
        return false; // already writable enough
    }
    set_file_read_only(path, readonly);
    true
}

/// `FileSystem::setFolderPermissions` (Windows): the files' read-only
/// attribute and ACL of the folder object, then a deny ACE (or none) in the
/// folder's DACL. As upstream, the change is always reported.
pub fn set_folder_permissions_changed(
    path: &str,
    permissions: super::FolderPermissions,
    permissions_changed: &mut bool,
) -> bool {
    *permissions_changed = false;
    if fs::metadata(long_win_path(path)).is_err() {
        return false;
    }
    let read_only = permissions == super::FolderPermissions::ReadOnly;
    // current read-only folder ACL needs to be removed from files also when
    // making a folder read-write
    set_file_read_only(path, read_only);
    nc_platform::windows::set_acl_permission(path, read_only);
    *permissions_changed = true;
    true
}

/// `FileSystem::isFolderReadOnly`: an access denied ACE in the DACL.
pub fn is_folder_read_only(path: &str) -> bool {
    nc_platform::windows::has_access_denied_ace(path)
}

/// `FileSystem::isFileLocked`: opens the file with the share mode of `mode`
/// and tries to lock it (`filesystembase.cpp:676-766`). A sharing or lock
/// violation means locked.
pub fn is_file_locked_mode(path: &str, mode: LockMode) -> bool {
    let mut options = fs::OpenOptions::new();
    match mode {
        LockMode::Exclusive => options.read(true).write(true).share_mode(0),
        LockMode::Shared => options.read(true).write(true).share_mode(7),
        LockMode::SharedRead => options.read(true).share_mode(7),
    };
    options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    let native = long_win_path(path);
    if fs::symlink_metadata(&native).is_err() {
        return false;
    }
    match options.open(&native) {
        Ok(file) => {
            // LockFile() is unsupported for directory handles, and opening
            // the directory already ruled out a sharing violation.
            if file.metadata().is_ok_and(|m| m.is_dir()) {
                return false;
            }
            match file.try_lock() {
                Ok(()) => {
                    let _ = file.unlock();
                    false
                }
                Err(fs::TryLockError::WouldBlock) => true,
                Err(fs::TryLockError::Error(e)) => {
                    log::warn!(target: "nextcloud.sync.filesystem", "lock file {path} {mode:?}: {e}");
                    false
                }
            }
        }
        Err(e) => match e.raw_os_error() {
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => true,
            _ => {
                log::warn!(target: "nextcloud.sync.filesystem", "lock file {path} {mode:?}: {e}");
                false
            }
        },
    }
}

/// `FileSystem::setFileHidden`.
pub fn set_file_hidden(path: &str, hidden: bool) {
    nc_platform::windows::set_file_hidden(path, hidden);
}

/// `Utility::freeDiskSpace`: available bytes, -1 if unknown.
pub fn free_disk_space(path: &str) -> i64 {
    nc_platform::windows::available_space(path).map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX))
}
