// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! Win32 calls that have no safe wrapper in the standard library.
//!
//! The directory watcher is a port of upstream's
//! `src/gui/folderwatcher_win.cpp` (nextcloud/desktop v34.0.5).

use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NOTIFY_ENUM_DIR, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileAttributesW, GetFileInformationByHandle, GetLongPathNameW,
    GetVolumeInformationW, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, ReadDirectoryChangesW,
    SetFileAttributesW,
};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    ResetEvent, SetEvent, WaitForMultipleObjects,
};

use crate::DirEvent;
use crate::paths::{from_long_win_path, long_win_path};

const LOG: &str = "nextcloud.gui.folderwatcher";

/// `STILL_ACTIVE` as returned by `GetExitCodeProcess`.
const STILL_ACTIVE: u32 = 259;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// The DNS host name of the machine.
pub fn computer_name() -> String {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is valid for `len` UTF-16 units; `len` is updated to
    // the number written, without the terminating NUL.
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// Whether the process `pid` is still running.
pub fn process_is_alive(pid: u32) -> bool {
    // SAFETY: plain query; the returned handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // SAFETY: reads the calling thread's last error.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut code = 0u32;
    // SAFETY: `handle` is a valid process handle with query access and
    // `code` outlives the call.
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    // SAFETY: `handle` was opened above and is not used afterwards.
    unsafe { CloseHandle(handle) };
    ok != 0 && code == STILL_ACTIVE
}

/// `FileSystem::setFileHidden`: sets or clears `FILE_ATTRIBUTE_HIDDEN`.
pub fn set_file_hidden(path: &str, hidden: bool) -> bool {
    let w = wide(&long_win_path(path));
    // SAFETY: `w` is a NUL-terminated UTF-16 string that outlives the calls.
    unsafe {
        let attrs = GetFileAttributesW(w.as_ptr());
        if attrs == INVALID_FILE_ATTRIBUTES {
            return false;
        }
        let new = if hidden {
            attrs | FILE_ATTRIBUTE_HIDDEN
        } else {
            attrs & !FILE_ATTRIBUTE_HIDDEN
        };
        new == attrs || SetFileAttributesW(w.as_ptr(), new) != 0
    }
}

/// What `csync_vio_local_stat` reads with `GetFileInformationByHandle`
/// (`csync_vio_local_win.cpp:198-224`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// `nFileIndexHigh:nFileIndexLow`, the inode of the journal.
    pub file_index: u64,
    pub attributes: u32,
}

/// `GetFileInformationByHandle` on the entry itself (reparse points are not
/// followed, like `FILE_FLAG_OPEN_REPARSE_POINT`).
pub fn file_info(path: &str) -> std::io::Result<FileInfo> {
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(long_win_path(path))?;
    // SAFETY: `BY_HANDLE_FILE_INFORMATION` is plain data, so all zeroes is
    // valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle belongs to `file`, open until the end of the
    // function, and `info` outlives the call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileInfo {
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        attributes: info.dwFileAttributes,
    })
}

/// `FileSystem::fileSystemForPath`: the name of the file system of the
/// volume holding `path` (`NTFS`, `FAT32`, `exFAT`, ...).
pub fn file_system_for_path(path: &str) -> Option<String> {
    let native = long_win_path(path);
    // The volume root: `\\?\C:\`, or `\\?\UNC\server\share\`.
    let root = if let Some(unc) = native.strip_prefix("\\\\?\\UNC\\") {
        let mut parts = unc.splitn(3, '\\');
        let (server, share) = (parts.next()?, parts.next()?);
        format!("\\\\?\\UNC\\{server}\\{share}\\")
    } else {
        let rest = native.strip_prefix("\\\\?\\")?;
        format!("\\\\?\\{}\\", rest.get(..2)?)
    };
    let w = wide(&root);
    let mut name = [0u16; 64];
    // SAFETY: `w` is NUL-terminated; only the file system name buffer is
    // requested, its length is passed.
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

/// A raw handle that may cross threads (event handles are process wide).
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// SAFETY: Win32 event and directory handles can be used from any thread.
unsafe impl Send for SendHandle {}
// SAFETY: see above.
unsafe impl Sync for SendHandle {}

/// `WatcherThread`: one recursive `ReadDirectoryChangesW` on a directory,
/// on a thread of its own. Dropping it stops the thread.
pub struct DirWatcher {
    stop_event: SendHandle,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DirWatcher {
    /// Starts watching `root` (a `/`-separated absolute path). `sink` is
    /// called on the watcher thread.
    pub fn start(root: &str, sink: impl Fn(DirEvent) + Send + 'static) -> std::io::Result<Self> {
        // SAFETY: manual-reset, initially non-signalled, unnamed event.
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if stop.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let stop_event = SendHandle(stop);
        let done = Arc::new(AtomicBool::new(false));
        let root = root.to_owned();
        let thread = std::thread::Builder::new()
            .name("nc-dir-watcher".into())
            .spawn({
                let done = done.clone();
                move || run(&root, stop_event, &done, &sink)
            });
        match thread {
            Ok(thread) => Ok(Self {
                stop_event,
                done,
                thread: Some(thread),
            }),
            Err(e) => {
                // SAFETY: the event was created above and nobody uses it.
                unsafe { CloseHandle(stop) };
                Err(e)
            }
        }
    }
}

impl Drop for DirWatcher {
    /// `WatcherThread::stop()` then `wait()`.
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        // SAFETY: the event stays valid until closed below.
        unsafe { SetEvent(self.stop_event.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: the thread is gone, nobody uses the event any more.
        unsafe { CloseHandle(self.stop_event.0) };
    }
}

/// `WatcherThread::run`.
fn run(root: &str, stop_event: SendHandle, done: &AtomicBool, sink: &dyn Fn(DirEvent)) {
    // SAFETY: manual-reset, initially non-signalled, unnamed event.
    let result_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if result_event.is_null() {
        log::warn!(target: LOG, "CreateEvent failed: {}", std::io::Error::last_os_error());
        return;
    }
    // If this buffer fills up before we've extracted its data we will lose
    // change information. Therefore start big.
    let mut buffer_size = 4096 * 10;
    const MAX_BUFFER: usize = 64 * 1024;
    while !done.load(Ordering::SeqCst) {
        let mut increase = false;
        watch_changes(
            root,
            buffer_size,
            &mut increase,
            result_event,
            stop_event,
            done,
            sink,
        );
        if increase {
            buffer_size = (buffer_size * 2).min(MAX_BUFFER);
        } else if !done.load(Ordering::SeqCst) {
            // Other errors shouldn't actually happen, so sleep a bit to
            // avoid running into the same error case in a tight loop.
            let mut waited = 0;
            while waited < 20 && !done.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                waited += 1;
            }
        }
    }
    // SAFETY: created above, not used after the loop.
    unsafe { CloseHandle(result_event) };
}

/// `WatcherThread::watchChanges`.
fn watch_changes(
    root: &str,
    buffer_size: usize,
    increase_buffer_size: &mut bool,
    result_event: HANDLE,
    stop_event: SendHandle,
    done: &AtomicBool,
    sink: &dyn Fn(DirEvent),
) {
    let long_path = long_win_path(root.trim_end_matches(['/', '\\']));
    let wpath = wide(&long_path);
    // SAFETY: `wpath` is NUL-terminated and outlives the call.
    let directory = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_WRITE | FILE_SHARE_READ | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if directory == INVALID_HANDLE_VALUE {
        log::warn!(target: LOG, "Failed to create handle for {root}, error: {}", std::io::Error::last_os_error());
        return;
    }

    // SAFETY: `OVERLAPPED` is plain data, so all zeroes is valid.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = result_event;

    // u64 elements: FILE_NOTIFY_INFORMATION needs DWORD alignment.
    let mut buffer = vec![0u64; buffer_size.div_ceil(8)];
    let root_clean = root.trim_end_matches(['/', '\\']).replace('\\', "/");

    while !done.load(Ordering::SeqCst) {
        // SAFETY: `result_event` is a valid event.
        unsafe { ResetEvent(result_event) };

        let mut bytes_returned = 0u32;
        // SAFETY: `directory` is a valid overlapped directory handle,
        // `buffer` and `overlapped` stay alive and unmoved until the
        // operation is finished or cancelled (`CancelIo` below, and the
        // wait for completion before the buffer is read).
        let ok = unsafe {
            ReadDirectoryChangesW(
                directory,
                buffer.as_mut_ptr().cast(),
                buffer_size as u32,
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES, // attributes are for vfs pin state changes
                &mut bytes_returned,
                &mut overlapped,
                None,
            )
        };
        if ok == 0 {
            // SAFETY: reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            if code == ERROR_NOTIFY_ENUM_DIR {
                log::debug!(target: LOG, "The buffer for changes overflowed! Triggering a generic change and resizing");
                sink(DirEvent::Changed(root_clean.clone()));
                *increase_buffer_size = true;
            } else {
                log::warn!(target: LOG, "ReadDirectoryChangesW error {}", std::io::Error::from_raw_os_error(code as i32));
            }
            break;
        }

        sink(DirEvent::Ready);

        let handles = [result_event, stop_event.0];
        // SAFETY: both handles are valid events.
        let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if result == WAIT_OBJECT_0 + 1 {
            log::debug!(target: LOG, "Received stop event, aborting folder watcher thread");
            break;
        }
        if result != WAIT_OBJECT_0 {
            log::warn!(target: LOG, "WaitForMultipleObjects failed {result} {}", std::io::Error::last_os_error());
            break;
        }

        // SAFETY: the operation on `overlapped` signalled completion.
        let ok = unsafe { GetOverlappedResult(directory, &overlapped, &mut bytes_returned, 0) };
        if ok == 0 {
            // SAFETY: reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            if code == ERROR_NOTIFY_ENUM_DIR {
                log::debug!(target: LOG, "The buffer for changes overflowed! Triggering a generic change and resizing");
                sink(DirEvent::LostChanges);
                sink(DirEvent::Changed(root_clean.clone()));
                *increase_buffer_size = true;
            } else {
                log::warn!(target: LOG, "GetOverlappedResult error {}", std::io::Error::from_raw_os_error(code as i32));
            }
            break;
        }

        let base: *const u8 = buffer.as_ptr().cast();
        let mut offset = 0usize;
        loop {
            // SAFETY: the kernel filled `bytes_returned` bytes of `buffer`
            // with a chain of DWORD-aligned FILE_NOTIFY_INFORMATION entries,
            // and `offset` follows `NextEntryOffset` inside that chain.
            let entry = unsafe { &*base.add(offset).cast::<FILE_NOTIFY_INFORMATION>() };
            let units = (entry.FileNameLength / 2) as usize;
            // SAFETY: `FileNameLength` bytes of UTF-16 follow the header.
            let name = unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), units) };
            let mut longfile = format!(
                "{long_path}\\{}",
                std::ffi::OsString::from_wide(name).to_string_lossy()
            );

            // Unless the file was removed or renamed, get its full long name
            // (expands 8.3 short names).
            if entry.Action != FILE_ACTION_REMOVED && entry.Action != FILE_ACTION_RENAMED_OLD_NAME {
                longfile = expand_long_name(&longfile);
            }
            let longfile = from_long_win_path(&longfile);

            // Skip modifications of folders: One of these is triggered for
            // changes and new files in a folder, probably because of the
            // folder's mtime changing. We don't need them.
            let skip =
                entry.Action == FILE_ACTION_MODIFIED && std::path::Path::new(&longfile).is_dir();
            if skip {
                log::debug!(target: LOG, "Skipping syncing of {longfile}");
            } else {
                sink(DirEvent::Changed(longfile));
            }

            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }

    // SAFETY: cancels the pending operation, if any, before the buffer and
    // the OVERLAPPED are dropped; the handle is closed once.
    unsafe {
        CancelIo(directory);
        CloseHandle(directory);
    }
}

/// `GetLongPathNameW`, keeping the name as it is when that fails.
fn expand_long_name(path: &str) -> String {
    let w = wide(path);
    let mut buf = vec![0u16; 4096];
    // SAFETY: `w` is NUL-terminated; `buf` is valid for its length.
    let n = unsafe { GetLongPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n > 0 && (n as usize) < buf.len() {
        return String::from_utf16_lossy(&buf[..n as usize]);
    }
    // SAFETY: reads the calling thread's last error.
    let error = unsafe { GetLastError() };
    if error != ERROR_FILE_NOT_FOUND && n == 0 {
        log::warn!(target: LOG, "Error converting file name {path} to full length, keeping original name. {}", std::io::Error::from_raw_os_error(error as i32));
    }
    path.to_owned()
}

/// `Utility::freeDiskSpace` (`GetDiskFreeSpaceExW`): the bytes available to
/// the caller on the volume of `path`.
pub fn available_space(path: &str) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w = wide(&long_win_path(path));
    let mut available = 0u64;
    // SAFETY: `w` is NUL-terminated; only the first out pointer is wanted.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

/// `c_utimes` (`UP/src/csync/std/c_time.cpp:50-62`): sets the access and
/// modification time of a file or directory (`SetFileTime` on a handle
/// opened with `FILE_FLAG_BACKUP_SEMANTICS`, which `filetime` does not use
/// for directories).
pub fn set_file_times(path: &str, unix_seconds: i64) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::{FILE_WRITE_ATTRIBUTES, SetFileTime};
    let ticks = (unix_seconds + 11_644_473_600) * 10_000_000;
    let ticks = u64::try_from(ticks).map_err(|_| std::io::Error::other("time out of range"))?;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(long_win_path(path))?;
    // SAFETY: the handle belongs to `file`, open for the whole call, and
    // `ft` outlives it.
    let ok = unsafe { SetFileTime(file.as_raw_handle(), std::ptr::null(), &ft, &ft) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `FindFirstFileW` / `FindNextFileW` on `path` (no wildcard): the names as
/// the file system stores them, so a lookup by another case answers with the
/// real name (`OwncloudPropagator::localFileNameClash` and
/// `hasCaseClashAccessibilityProblem`). Empty when nothing matches.
pub fn find_file_names(path: &str) -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
    };
    let w = wide(&long_win_path(path));
    // SAFETY: `WIN32_FIND_DATAW` is plain data, so all zeroes is valid.
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: `w` is NUL-terminated and `data` outlives the call.
    let handle = unsafe { FindFirstFileW(w.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let name = |d: &WIN32_FIND_DATAW| {
        let len = d
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(d.cFileName.len());
        String::from_utf16_lossy(&d.cFileName[..len])
    };
    let mut names = vec![name(&data)];
    // SAFETY: `handle` comes from the successful FindFirstFileW above and is
    // closed once; `data` outlives each call.
    unsafe {
        while FindNextFileW(handle, &mut data) != 0 {
            names.push(name(&data));
        }
        FindClose(handle);
    }
    names
}

/// `ACCESS_DENIED_ACE_TYPE`.
const ACCESS_DENIED_ACE_TYPE: u8 = 1;

/// A security descriptor returned by `GetSecurityInfo`, freed with
/// `LocalFree`.
struct LocalSecurity(*mut std::ffi::c_void);

impl Drop for LocalSecurity {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by GetSecurityInfo / ConvertStringSidToSidW,
            // which documents LocalFree as the way to release it; freed once.
            unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
        }
    }
}

/// Opens `path` the way upstream's ACL code does: no content access, the
/// object itself (reparse points are not followed), directories included.
fn open_for_security(path: &str, access: u32) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(long_win_path(path))
}

/// The DACL of an open object and the descriptor that owns it.
fn dacl_of(
    file: &std::fs::File,
) -> Result<(*mut windows_sys::Win32::Security::ACL, LocalSecurity), u32> {
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the handle belongs to `file`; the out pointers are valid. The
    // DACL points into the descriptor, which the guard keeps alive.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    let guard = LocalSecurity(descriptor);
    if error != 0 {
        return Err(error);
    }
    Ok((dacl, guard))
}

/// `FileSystem::isFolderReadOnly` on Windows: the DACL has an access denied
/// ACE (`UP/src/libsync/filesystem.cpp:713-770`). False when it cannot be read.
pub fn has_access_denied_ace(path: &str) -> bool {
    use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
    use windows_sys::Win32::Security::{ACE_HEADER, AclSizeInformation, GetAce, GetAclInformation};
    const READ_CONTROL: u32 = 0x0002_0000;
    let file = match open_for_security(path, READ_CONTROL) {
        Ok(f) => f,
        Err(e) => {
            log::warn!(target: "nextcloud.sync.filesystem", "CreateFileW failed, path={path} errorMessage={e}");
            return false;
        }
    };
    let Ok((dacl, _descriptor)) = dacl_of(&file) else {
        return false;
    };
    if dacl.is_null() {
        return false;
    }
    // SAFETY: plain data, all zeroes is valid.
    let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `dacl` is valid while `_descriptor` lives; `size` is the right size.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            std::ptr::addr_of_mut!(size).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return false;
    }
    for i in 0..size.AceCount {
        let mut ace = std::ptr::null_mut();
        // SAFETY: `i` is below the ACE count of a valid ACL.
        if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
            return false;
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let ace_type = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
        if ace_type & ACCESS_DENIED_ACE_TYPE != 0 {
            return true;
        }
    }
    false
}

/// `FileSystem::setAclPermission`: rebuilds the DACL of a file or folder
/// without its access denied ACEs and, for read-only, with one in front that
/// forbids deleting, creating and writing for BUILTIN\Users
/// (`UP/src/common/filesystembase.cpp:823-977`).
pub fn set_acl_permission(path: &str, read_only: bool) -> bool {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        AddAccessDeniedAceEx, AddAce, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
        GetLengthSid, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    const NO_PROPAGATE_INHERIT_ACE: u32 = 0x4;
    // FILE_DELETE_CHILD | DELETE | FILE_WRITE_DATA | FILE_WRITE_EA | FILE_APPEND_DATA
    const DENIED_MASK: u32 = 0x40 | 0x1_0000 | 0x2 | 0x10 | 0x4;
    let warn = |what: &str, detail: &dyn std::fmt::Display| {
        log::warn!(target: "nextcloud.sync.filesystem", "{what} failed, path={path} errorMessage={detail}");
    };

    let file = match open_for_security(path, READ_CONTROL | WRITE_DAC | MAXIMUM_ALLOWED) {
        Ok(f) => f,
        Err(e) => {
            warn("CreateFileW", &e);
            return false;
        }
    };
    let (dacl, _descriptor) = match dacl_of(&file) {
        Ok(v) => v,
        Err(code) => {
            warn(
                "GetSecurityInfo",
                &std::io::Error::from_raw_os_error(code as i32),
            );
            return false;
        }
    };
    if dacl.is_null() {
        warn("GetSecurityInfo", &"no DACL");
        return false;
    }
    let sid_text = wide("S-1-5-32-545"); // BUILTIN\Users
    let mut sid = std::ptr::null_mut();
    // SAFETY: `sid_text` is NUL-terminated; the SID is freed by the guard.
    if unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) } == 0 {
        warn("ConvertStringSidToSidW", &std::io::Error::last_os_error());
        return false;
    }
    let _sid_guard = LocalSecurity(sid);

    // SAFETY: plain data, all zeroes is valid.
    let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `dacl` is valid while `_descriptor` lives.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            std::ptr::addr_of_mut!(size).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        warn("GetAclInformation", &std::io::Error::last_os_error());
        return false;
    }

    let mut new_acl_size = size.AclBytesInUse;
    if read_only {
        // Room for the ACCESS_DENIED_ACE without its SidStart member, plus
        // the SID; an ACL cannot exceed 65532 bytes (older clients prepended a
        // new ACE every time, so an ACL that is already that big is kept).
        // SAFETY: `sid` is a valid SID.
        let sid_len = unsafe { GetLengthSid(sid) };
        let ace_size = (std::mem::size_of::<ACCESS_DENIED_ACE>() - std::mem::size_of::<u32>())
            as u32
            + sid_len;
        if new_acl_size + ace_size < 65532 {
            new_acl_size += ace_size;
        }
    }
    // u32 elements: an ACL needs DWORD alignment.
    let mut buffer = vec![0u32; (new_acl_size as usize).div_ceil(4)];
    let new_dacl: *mut ACL = buffer.as_mut_ptr().cast();
    // SAFETY: `new_dacl` points to at least `new_acl_size` writable bytes.
    if unsafe { InitializeAcl(new_dacl, new_acl_size, ACL_REVISION) } == 0 {
        warn("InitializeAcl", &std::io::Error::last_os_error());
        return false;
    }
    let mut new_ace_index = 0u32;
    if read_only {
        // the access denied ACE needs to appear at the start of the ACL
        // SAFETY: `new_dacl` was initialized above and has room for the ACE.
        let ok = unsafe {
            AddAccessDeniedAceEx(
                new_dacl,
                ACL_REVISION,
                NO_PROPAGATE_INHERIT_ACE,
                DENIED_MASK,
                sid,
            )
        };
        if ok == 0 {
            warn("AddAccessDeniedAceEx", &std::io::Error::last_os_error());
            return false;
        }
        new_ace_index += 1;
    }
    for i in 0..size.AceCount {
        let mut ace = std::ptr::null_mut();
        // SAFETY: `i` is below the ACE count of the old, valid ACL.
        if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
            warn("GetAce", &std::io::Error::last_os_error());
            return false;
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType & ACCESS_DENIED_ACE_TYPE != 0 {
            // skip any access denied ACEs from the previous ACL
            continue;
        }
        // SAFETY: the ACE is `AceSize` bytes long and `new_dacl` has room.
        if unsafe {
            AddAce(
                new_dacl,
                ACL_REVISION,
                new_ace_index,
                ace,
                u32::from(header.AceSize),
            )
        } == 0
        {
            warn("AddAce", &std::io::Error::last_os_error());
            return false;
        }
        new_ace_index += 1;
    }
    // SAFETY: the handle belongs to `file`; `new_dacl` is a valid ACL that
    // outlives the call.
    let error = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            PROTECTED_DACL_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | READ_CONTROL
                | WRITE_DAC,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    if error != 0 {
        warn(
            "SetSecurityInfo",
            &std::io::Error::from_raw_os_error(error as i32),
        );
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        AddAccessDeniedAceEx, AddAce, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
        GetLengthSid, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;

    fn acl_size(file: &std::fs::File) -> ACL_SIZE_INFORMATION {
        let (dacl, _d) = dacl_of(file).expect("GetSecurityInfo");
        // SAFETY: plain data.
        let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `dacl` stays valid while `_d` lives.
        let ok = unsafe {
            GetAclInformation(
                dacl,
                std::ptr::addr_of_mut!(size).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        assert_ne!(ok, 0);
        size
    }

    // testfilesystem.cpp: testAclWithManyDeniedAces. Regression from client
    // versions < 4.0.2 (nextcloud/desktop#8860): the access denied ACE was
    // added every time a file/folder was set to read-only, until the ACL was
    // too big to be modified.
    #[test]
    fn acl_with_many_denied_aces() {
        let dir = tempfile::tempdir().unwrap();
        let test = dir.path().join("readonlyTest");
        std::fs::create_dir(&test).unwrap();
        let path = test.to_string_lossy().into_owned();

        let file = open_for_security(&path, READ_CONTROL | WRITE_DAC | MAXIMUM_ALLOWED).unwrap();
        let sid_text = wide("S-1-5-32-545");
        let mut sid = std::ptr::null_mut();
        // SAFETY: NUL-terminated string; the SID is freed by the guard.
        let ok = unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) };
        assert_ne!(ok, 0);
        let _sid = LocalSecurity(sid);

        let initial = acl_size(&file);
        // SAFETY: valid SID.
        let sid_len = unsafe { GetLengthSid(sid) };
        let ace_size = (std::mem::size_of::<ACCESS_DENIED_ACE>() - 4) as u32 + sid_len;
        // ACLs can be up to 65532 bytes in size: add as many ACEs as possible
        let count = (65532 - initial.AclBytesInUse) / ace_size;
        let total = initial.AclBytesInUse + ace_size * count;
        let mut buffer = vec![0u32; (total as usize).div_ceil(4)];
        let new_dacl: *mut ACL = buffer.as_mut_ptr().cast();
        // SAFETY: the buffer holds `total` bytes.
        let ok = unsafe { InitializeAcl(new_dacl, total, ACL_REVISION) };
        assert_ne!(ok, 0);
        let mut index = 0;
        for _ in 0..count {
            // SAFETY: room for `count` ACEs was reserved.
            let ok = unsafe { AddAccessDeniedAceEx(new_dacl, ACL_REVISION, 4, 0x1_0042, sid) };
            assert_ne!(ok, 0);
            index += 1;
        }
        let (old, _d) = dacl_of(&file).unwrap();
        for i in 0..initial.AceCount {
            let mut ace = std::ptr::null_mut();
            // SAFETY: `i` is below the old ACL's ACE count.
            let ok = unsafe { GetAce(old, i, &mut ace) };
            assert_ne!(ok, 0);
            // SAFETY: ACEs start with a header.
            let size = u32::from(unsafe { (*ace.cast::<ACE_HEADER>()).AceSize });
            // SAFETY: valid ACE, room reserved.
            let ok = unsafe { AddAce(new_dacl, ACL_REVISION, index, ace, size) };
            assert_ne!(ok, 0);
            index += 1;
        }
        // SAFETY: valid handle and ACL.
        let error = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                PROTECTED_DACL_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | READ_CONTROL
                    | WRITE_DAC,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(error, 0);

        // Test setup done, so let's verify the behaviour.
        let duplicates = acl_size(&file);
        assert!(duplicates.AceCount > initial.AceCount);
        assert!(duplicates.AclBytesInUse > 65511);
        assert!(has_access_denied_ace(&path));

        // Try to mark the folder as readonly. This used to fail because the
        // ACL size was always extended, even if it wasn't necessary.
        assert!(set_acl_permission(&path, true));
        let after_read_only = acl_size(&file);
        assert!(after_read_only.AceCount > initial.AceCount);
        assert!(after_read_only.AceCount < duplicates.AceCount);
        assert!(after_read_only.AclBytesInUse < 512);
        assert!(has_access_denied_ace(&path));

        // Finally, remove the read-only ACL: the same ACEs as before the test.
        assert!(set_acl_permission(&path, false));
        let after_read_write = acl_size(&file);
        assert_eq!(after_read_write.AceCount, initial.AceCount);
        assert_eq!(after_read_write.AclBytesInUse, initial.AclBytesInUse);
        assert!(!has_access_denied_ace(&path));
    }
}
