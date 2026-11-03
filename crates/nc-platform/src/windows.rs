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

/// What the directory watcher reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirEvent {
    /// `ready()`: the first `ReadDirectoryChangesW` is armed.
    Ready,
    /// `changed(path)`: an absolute, `/`-separated path.
    Changed(String),
    /// `lostChanges()`: the notification buffer overflowed.
    LostChanges,
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
