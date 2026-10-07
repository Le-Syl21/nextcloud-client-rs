// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/folderwatcher_linux.{h,cpp}` (nextcloud/desktop
// v34.0.5).

//! `FolderWatcherPrivate`: the Linux (inotify) implementation of the
//! folder watcher.
//!
//! One inotify watch per directory of the tree, with upstream's exact mask.
//! Watches are added recursively for new or moved-in directories and removed
//! for deleted or moved-away ones. The inotify system calls go through the
//! [`InotifySys`] seam so tests can observe or fake them; the real
//! implementation is [`LinuxInotify`] over the `inotify` crate. Reading the
//! event buffer and decoding `struct inotify_event` is ported as is (the
//! `inotify` crate's own reader cannot be fed a fake descriptor, which
//! `testinotifywatcher` does with a pipe).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use inotify::{EventMask, WatchDescriptor, WatchMask, Watches};
use log::{debug, warn};

const LOG: &str = "nextcloud.gui.folderwatcher";

/// `sizeof(struct inotify_event)`: `int wd; uint32_t mask, cookie, len;`.
pub const INOTIFY_EVENT_SIZE: usize = 16;

/// The mask of every watch, as `inotifyRegisterPath` passes it:
/// `IN_CLOSE_WRITE | IN_ATTRIB | IN_MOVE | IN_CREATE | IN_DELETE |
/// IN_DELETE_SELF | IN_MOVE_SELF | IN_UNMOUNT | IN_ONLYDIR`.
pub fn watch_mask() -> WatchMask {
    WatchMask::CLOSE_WRITE
        | WatchMask::ATTRIB
        | WatchMask::MOVE
        | WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        // `WatchMask` has no `UNMOUNT` (it is an event bit), but upstream
        // passes `IN_UNMOUNT` and the kernel accepts it.
        | WatchMask::from_bits_retain(EventMask::UNMOUNT.bits())
        | WatchMask::ONLYDIR
}

/// The inotify system calls `FolderWatcherPrivate` makes on its descriptor
/// (`inotify_add_watch`, `inotify_rm_watch`), as a seam for tests.
pub trait InotifySys {
    /// `inotify_add_watch(fd, path, mask)`: the watch descriptor, or the
    /// `errno` as an [`io::Error`].
    fn add_watch(&mut self, path: &str, mask: WatchMask) -> io::Result<i32>;
    /// `inotify_rm_watch(fd, wd)` (errors are ignored, like upstream).
    fn rm_watch(&mut self, wd: i32);
}

/// The real inotify descriptor, through the `inotify` crate.
#[derive(Debug)]
pub struct LinuxInotify {
    watches: Watches,
    descriptors: HashMap<i32, WatchDescriptor>,
}

impl LinuxInotify {
    /// Wraps the watches handle of an `inotify::Inotify` (from
    /// `Inotify::watches`).
    pub fn new(watches: Watches) -> Self {
        Self {
            watches,
            descriptors: HashMap::new(),
        }
    }
}

impl InotifySys for LinuxInotify {
    fn add_watch(&mut self, path: &str, mask: WatchMask) -> io::Result<i32> {
        let wd = self.watches.add(path, mask)?;
        let id = wd.get_watch_descriptor_id();
        self.descriptors.insert(id, wd);
        Ok(id)
    }

    fn rm_watch(&mut self, wd: i32) {
        if let Some(descriptor) = self.descriptors.remove(&wd) {
            let _ = self.watches.remove(descriptor);
        }
    }
}

/// No inotify descriptor: what upstream does with `_fd` = -1 (when
/// `inotify_init()` failed) or 0 (default constructor): every
/// `inotify_add_watch` fails with `EBADF`.
#[derive(Debug, Default)]
pub struct NoInotify;

impl InotifySys for NoInotify {
    fn add_watch(&mut self, _path: &str, _mask: WatchMask) -> io::Result<i32> {
        Err(io::Error::from_raw_os_error(
            rustix::io::Errno::BADF.raw_os_error(),
        ))
    }

    fn rm_watch(&mut self, _wd: i32) {}
}

/// What `FolderWatcherPrivate` uses of its `_parent` (`FolderWatcher`).
/// `()` is the null parent of the default-constructed private
/// (upstream `_parent = nullptr`).
pub trait WatcherParent {
    /// `_parent->changeDetected(path)`.
    fn change_detected(&mut self, path: &str);
    /// `_parent->pathIsIgnored(path)`.
    fn path_is_ignored(&self, path: &str) -> bool;
    /// `inotify_add_watch` failed with `ENOMEM` or `ENOSPC`: if the parent
    /// is still reliable, it becomes unreliable and emits `becameUnreliable`.
    fn watches_exhausted(&mut self);
    /// The kernel dropped events (`IN_Q_OVERFLOW`): `lostChanges`. Not in
    /// upstream's Linux implementation (see the crate documentation).
    fn lost_changes(&mut self);
}

impl WatcherParent for () {
    fn change_detected(&mut self, _path: &str) {}
    fn path_is_ignored(&self, path: &str) -> bool {
        path.is_empty()
    }
    fn watches_exhausted(&mut self) {}
    fn lost_changes(&mut self) {}
}

/// `QDir(path).absolutePath()`: absolute (relative paths are taken from the
/// current directory), without a trailing `/`. Not canonicalized.
fn absolute_path(path: &str) -> String {
    let mut p = if path.starts_with('/') {
        path.to_owned()
    } else {
        let cwd = std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_empty() || path == "." {
            cwd
        } else {
            format!("{}/{path}", cwd.trim_end_matches('/'))
        }
    };
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// `QDir(path).path()`: the path without a trailing `/`.
fn dir_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        "/"
    } else {
        trimmed
    }
}

/// `QDir::exists() && QDir::isReadable()`.
fn dir_exists_and_readable(path: &str) -> bool {
    Path::new(path).is_dir() && rustix::fs::access(path, rustix::fs::Access::READ_OK).is_ok()
}

/// `QDir::entryList({"*"}, QDir::Dirs | QDir::NoDotAndDotDot |
/// QDir::NoSymLinks | QDir::Hidden)`: the sub directories (hidden ones
/// included, symlinks excluded), sorted like `QDir::Name | QDir::IgnoreCase`.
fn sub_directories(path: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    names
}

/// One decoded `struct inotify_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawInotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    /// `event->len` (the name field size, padding included).
    pub len: u32,
    /// `event->name` up to its first NUL.
    pub name: Vec<u8>,
}

/// `FolderWatcherPrivate` (Linux).
pub struct FolderWatcherPrivate {
    sys: Box<dyn InotifySys>,
    folder: String,
    watch_to_path: HashMap<i32, String>,
    /// `QMap<QString, int>`: ordered, `removeFoldersBelow` relies on it.
    /// Byte order of UTF-8 keeps every path with a given prefix contiguous,
    /// like `QString` order does.
    path_to_watch: BTreeMap<String, i32>,
    /// On linux the watcher is ready when the constructor finished.
    pub ready: bool,
}

impl Default for FolderWatcherPrivate {
    /// `FolderWatcherPrivate() = default`: no descriptor, no folder.
    fn default() -> Self {
        Self {
            sys: Box::new(NoInotify),
            folder: String::new(),
            watch_to_path: HashMap::new(),
            path_to_watch: BTreeMap::new(),
            ready: true,
        }
    }
}

impl std::fmt::Debug for FolderWatcherPrivate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderWatcherPrivate")
            .field("folder", &self.folder)
            .field("watches", &self.path_to_watch.len())
            .finish()
    }
}

impl FolderWatcherPrivate {
    /// `FolderWatcherPrivate(p, path)`: watches `path` recursively right
    /// away (upstream's `invokeMethod` on the same thread is a direct call).
    pub fn new(sys: Box<dyn InotifySys>, path: &str, parent: &mut dyn WatcherParent) -> Self {
        let mut d = Self {
            sys,
            folder: path.to_owned(),
            ..Self::default()
        };
        d.slot_add_folder_recursive(path, parent);
        d
    }

    /// The root of the watched folder.
    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// `testWatchCount()`: the number of watched directories.
    pub fn test_watch_count(&self) -> usize {
        self.path_to_watch.len()
    }

    /// `testWatchContains(watchDescriptor)`: whether the watch descriptor is
    /// registered.
    pub fn test_watch_contains(&self, watch_descriptor: i32) -> bool {
        self.watch_to_path.contains_key(&watch_descriptor)
    }

    /// `findFoldersBelow(dir, fullList)`: appends every directory below `dir`
    /// (depth first) to `full_list`. Like upstream, the result only reflects
    /// the traversal of the last sub directory.
    pub fn find_folders_below(&self, dir: &str, full_list: &mut Vec<String>) -> bool {
        let mut ok = true;
        if !dir_exists_and_readable(dir) {
            debug!(target: LOG, "Non existing path coming in: {}", absolute_path(dir));
            ok = false;
        } else {
            let base = dir_path(dir);
            for name in sub_directories(dir) {
                let full_path = format!("{base}/{name}");
                full_list.push(full_path.clone());
                ok = self.find_folders_below(&full_path, full_list);
            }
        }
        ok
    }

    /// `inotifyRegisterPath(path)`.
    fn inotify_register_path(&mut self, path: &str, parent: &mut dyn WatcherParent) {
        if path.is_empty() {
            return;
        }
        match self.sys.add_watch(path, watch_mask()) {
            Ok(wd) if wd > -1 => {
                self.watch_to_path.insert(wd, path.to_owned());
                self.path_to_watch.insert(path.to_owned(), wd);
            }
            Ok(_) => {}
            Err(e) => {
                // If we're running out of memory or inotify watches, become
                // unreliable.
                let errno = e.raw_os_error();
                if errno == Some(rustix::io::Errno::NOMEM.raw_os_error())
                    || errno == Some(rustix::io::Errno::NOSPC.raw_os_error())
                {
                    parent.watches_exhausted();
                }
            }
        }
    }

    /// `slotAddFolderRecursive(path)`.
    pub fn slot_add_folder_recursive(&mut self, path: &str, parent: &mut dyn WatcherParent) {
        if self.path_to_watch.contains_key(path) {
            return;
        }

        let mut subdirs = 0;
        debug!(target: LOG, "(+) Watcher: {path}");

        self.inotify_register_path(&absolute_path(path), parent);

        let mut all_subfolders = Vec::new();
        if !self.find_folders_below(path, &mut all_subfolders) {
            warn!(target: LOG, "Could not traverse all sub folders");
        }
        for subfolder in all_subfolders {
            let absolute = absolute_path(&subfolder);
            if Path::new(&subfolder).is_dir() && !self.path_to_watch.contains_key(&absolute) {
                subdirs += 1;
                if parent.path_is_ignored(&subfolder) {
                    debug!(target: LOG, "* Not adding {subfolder}");
                    continue;
                }
                self.inotify_register_path(&absolute, parent);
            } else {
                debug!(target: LOG, "    `-> discarded: {subfolder}");
            }
        }

        if subdirs > 0 {
            debug!(target: LOG, "    `-> and {subdirs} subdirectories");
        }
    }

    /// `slotReceivedNotification(fd)`: one `read(2)` of the descriptor and
    /// the handling of the events it returned. A read error (`EAGAIN` once
    /// the queue is drained) is returned without handling anything.
    pub fn slot_received_notification(
        &mut self,
        fd: BorrowedFd<'_>,
        parent: &mut dyn WatcherParent,
    ) -> io::Result<()> {
        let mut buffer = vec![0u8; 2048];
        let mut result = rustix::io::read(fd, &mut buffer[..]);
        // From inotify documentation:
        //
        // The behavior when the buffer given to read(2) is too small to return
        // information about the next event depends on the kernel version: in
        // kernels before 2.6.21, read(2) returns 0; since kernel 2.6.21,
        // read(2) fails with the error EINVAL.
        while result == Err(rustix::io::Errno::INVAL) {
            // double the buffer size and try again ...
            buffer.resize(buffer.len() * 2, 0);
            result = rustix::io::read(fd, &mut buffer[..]);
        }
        let len = result.map_err(io::Error::from)?;
        self.handle_events(&parse_inotify_events(&buffer[..len]), parent);
        Ok(())
    }

    /// The event loop of `slotReceivedNotification`, over decoded events.
    pub fn handle_events(&mut self, events: &[RawInotifyEvent], parent: &mut dyn WatcherParent) {
        for event in events {
            // Divergence: upstream skips IN_Q_OVERFLOW (its len is 0) and so
            // silently loses the dropped events until the next full local
            // discovery. Report it like the Windows watcher does.
            if event.mask & EventMask::Q_OVERFLOW.bits() != 0 {
                warn!(target: LOG, "inotify event queue overflowed, changes were lost");
                parent.lost_changes();
                continue;
            }

            // Fire event for the path that was changed.
            if event.len == 0 || event.wd <= -1 {
                continue;
            }
            let file_name = &event.name;
            // Filter out journal changes - redundant with filtering in
            // FolderWatcher::pathIsIgnored.
            if file_name.starts_with(b"._sync_")
                || file_name.starts_with(b".csync_journal.db")
                || file_name.starts_with(b".sync_")
            {
                continue;
            }
            let file_name = String::from_utf8_lossy(file_name);
            let Some(watch_path) = self.watch_to_path.get(&event.wd) else {
                debug!(target: LOG, "Ignoring event for unknown watch descriptor {} {file_name}", event.wd);
                continue;
            };

            let p = format!("{watch_path}/{file_name}");
            parent.change_detected(&p);

            if event.mask & (EventMask::MOVED_TO.bits() | EventMask::CREATE.bits()) != 0
                && Path::new(&p).is_dir()
                && !parent.path_is_ignored(&p)
            {
                self.slot_add_folder_recursive(&p, parent);
            }
            if event.mask & (EventMask::MOVED_FROM.bits() | EventMask::DELETE.bits()) != 0 {
                self.remove_folders_below(&p);
            }
        }
    }

    /// `removeFoldersBelow(path)`: removes the watch of `path` and of every
    /// directory below it.
    pub fn remove_folders_below(&mut self, path: &str) {
        if !self.path_to_watch.contains_key(path) {
            return;
        }

        let path_slash = format!("{path}/");

        // Remove the entry and all subentries; order is 'foo', 'foo bar',
        // 'foo/bar'.
        let to_remove: Vec<(String, i32)> = self
            .path_to_watch
            .range(path.to_owned()..)
            .take_while(|(k, _)| k.starts_with(path))
            .filter(|(k, _)| k.as_str() == path || k.starts_with(&path_slash))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (item_path, wid) in to_remove {
            self.sys.rm_watch(wid);
            self.watch_to_path.remove(&wid);
            self.path_to_watch.remove(&item_path);
            debug!(target: LOG, "Removed watch for {item_path}");
        }
    }
}

/// Decodes a buffer returned by `read(2)` on an inotify descriptor.
///
/// Upstream's loop runs while `i + sizeof(inotify_event) < len`, which never
/// decodes a nameless event ending the buffer; those are all skipped anyway
/// (`event->len == 0`) except `IN_Q_OVERFLOW`, which the port reports, so
/// the bound here is `<=`. A truncated name is cut at the end of the buffer.
pub fn parse_inotify_events(buffer: &[u8]) -> Vec<RawInotifyEvent> {
    let u32_at =
        |i: usize| u32::from_ne_bytes([buffer[i], buffer[i + 1], buffer[i + 2], buffer[i + 3]]);
    let mut events = Vec::new();
    let mut i = 0;
    while i + INOTIFY_EVENT_SIZE <= buffer.len() {
        let wd = u32_at(i) as i32;
        let mask = u32_at(i + 4);
        let cookie = u32_at(i + 8);
        let len = u32_at(i + 12);
        let start = i + INOTIFY_EVENT_SIZE;
        let end = start.saturating_add(len as usize).min(buffer.len());
        let raw_name = &buffer[start..end];
        let name = raw_name
            .split(|b| *b == 0)
            .next()
            .unwrap_or_default()
            .to_vec();
        events.push(RawInotifyEvent {
            wd,
            mask,
            cookie,
            len,
            name,
        });
        i = start.saturating_add(len as usize);
    }
    events
}
