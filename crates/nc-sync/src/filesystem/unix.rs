// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later

//! The Unix (Linux and macOS) side of the local file system layer.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use nc_journal::csync::ItemType;

use super::{FileStat, FolderPermissions, OpendirError};

/// The modification time of a followed `stat`.
pub(super) fn metadata_mtime(md: &fs::Metadata) -> i64 {
    md.mtime()
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
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        // UF_HIDDEN
        st.is_hidden = md.st_flags() & 0x8000 != 0;
    }
    if check_permissions_validity {
        st.is_permissions_invalid = md.mode() & 0o002 == 0o002;
    }
    Some(st)
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

/// `FileSystem::isWritable` (`QFileInfo::isWritable`).
pub fn is_writable(path: &str) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
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

pub(super) fn mode(path: &str) -> Option<u32> {
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

/// `FileSystem::setFolderPermissions(path, permissions, &permissionsChanged)`:
/// `permissions_changed` tells whether the mode was modified.
pub fn set_folder_permissions_changed(
    path: &str,
    permissions: FolderPermissions,
    permissions_changed: &mut bool,
) -> bool {
    *permissions_changed = false;
    let Some(current) = mode(path) else {
        log::warn!(target: "nextcloud.sync.filesystem", "exception when modifying folder permissions - path: {path}");
        return false;
    };
    let new = match permissions {
        FolderPermissions::ReadOnly => current & !0o222,
        FolderPermissions::ReadWrite => (current & !0o002) | 0o200,
    };
    if new != current {
        if fs::set_permissions(path, fs::Permissions::from_mode(new)).is_err() {
            log::warn!(target: "nextcloud.sync.filesystem", "exception when modifying folder permissions - path: {path}");
            return false;
        }
        *permissions_changed = true;
    }
    true
}

/// `FileSystem::isFolderReadOnly` (Unix): the owner cannot write.
pub fn is_folder_read_only(path: &str) -> bool {
    mode(path).is_some_and(|m| m & 0o200 != 0o200)
}

/// `FileSystem::isFileLocked`: only Windows has mandatory locks.
pub fn is_file_locked_mode(_path: &str, _mode: super::LockMode) -> bool {
    false
}

/// `FileSystem::setFileHidden`: a no-op outside Windows.
pub fn set_file_hidden(_path: &str, _hidden: bool) {}

/// `Utility::freeDiskSpace`: available bytes, -1 if unknown.
pub fn free_disk_space(path: &str) -> i64 {
    match rustix::fs::statvfs(path) {
        Ok(s) => (s.f_bavail as i64).saturating_mul(s.f_frsize as i64),
        Err(_) => -1,
    }
}
