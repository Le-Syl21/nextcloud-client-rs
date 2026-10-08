// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/folderwatcher.{h,cpp}` (nextcloud/desktop
// v34.0.5).

//! `FolderWatcher`: monitors a directory recursively for changes.
//!
//! The folder watcher monitors a directory and its sub directories for
//! changes in the local file system. Only the Linux (inotify) implementation
//! is ported ([`linux`]).
//!
//! # Runtime
//!
//! Like upstream's `QObject` on the GUI thread, the watcher is single
//! threaded: it must be created and used inside a tokio
//! [`LocalSet`](tokio::task::LocalSet) of a runtime with IO and time
//! enabled. [`FolderWatcher::init`] spawns a local task that reads the
//! inotify descriptor (upstream's `QSocketNotifier`); upstream's `QTimer`s
//! are local tasks too. Dropping the watcher stops everything.
//!
//! # Signals
//!
//! Upstream's signals are sent as [`FolderWatcherEvent`]s on the
//! `tokio::sync::mpsc` channel given to [`FolderWatcher::new`]:
//!
//! | Upstream signal | Event | `Folder` slot upstream connects |
//! |---|---|---|
//! | `pathChanged(path)` | [`FolderWatcherEvent::PathChanged`] | `slotWatchedPathChanged` (exclusion, then `LocalDiscoveryTracker::addTouchedPath`) |
//! | `lostChanges()` | [`FolderWatcherEvent::LostChanges`] | `slotNextSyncFullLocalDiscovery` |
//! | `becameUnreliable(message)` | [`FolderWatcherEvent::BecameUnreliable`] | `slotWatcherUnreliable` |
//! | `filesLockReleased(files)` | [`FolderWatcherEvent::FilesLockReleased`] | `slotFilesLockReleased` |
//! | `filesLockImposed(files)` | [`FolderWatcherEvent::FilesLockImposed`] | `slotFilesLockImposed` |
//! | `lockedFilesFound(files)` | [`FolderWatcherEvent::LockedFilesFound`] | `slotLockedFilesFound` |
//!
//! Paths are absolute (the watched root joined with the names inotify
//! reports), like upstream.
//!
//! # Divergences
//!
//! * `IN_Q_OVERFLOW` sends [`FolderWatcherEvent::LostChanges`]; upstream's
//!   Linux watcher ignores the overflow event (only its Windows watcher
//!   emits `lostChanges`), so dropped kernel events were lost until the next
//!   periodic full local discovery.
//! * Upstream's `Folder *` is not modelled: `folderAccountCapabilitiesChanged`
//!   takes the `filesLockAvailable` capability as an argument
//!   ([`FolderWatcher::folder_account_capabilities_changed`]), called by the
//!   owner when the capabilities change.

pub mod linux;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::fd::AsFd;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use inotify::Inotify;
use log::{debug, info, warn};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use nc_sync::filesystem;
use nc_sync::filesystem::lock_file::{self, FileLockingType};

use self::linux::{FolderWatcherPrivate, InotifySys, LinuxInotify, NoInotify, WatcherParent};

const LOG: &str = "nextcloud.gui.folderwatcher";

/// `lockChangeDebouncingTimerIntervalMs`.
const LOCK_CHANGE_DEBOUNCING_TIMER_INTERVAL: Duration = Duration::from_millis(500);

/// The message of `becameUnreliable` when inotify watches are exhausted.
pub const WATCHES_EXHAUSTED_MESSAGE: &str = "This problem usually happens when the inotify watches are exhausted. Check the FAQ for details.";

/// The message of `becameUnreliable` when the test notification never came.
pub const NO_TEST_NOTIFICATION_MESSAGE: &str = "The watcher did not receive a test notification.";

/// The signals of upstream's `FolderWatcher`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderWatcherEvent {
    /// `pathChanged(path)`: one of the watched directories or one of the
    /// contained files changed. The absolute path.
    PathChanged(String),
    /// `filesLockReleased(files)`: lock files were removed; the documents
    /// they guarded.
    FilesLockReleased(BTreeSet<String>),
    /// `filesLockImposed(files)`: lock files were added; the documents they
    /// guard.
    FilesLockImposed(BTreeSet<String>),
    /// `lockedFilesFound(files)`: sent right after `FilesLockImposed`, with
    /// the same files.
    LockedFilesFound(BTreeSet<String>),
    /// `lostChanges()`: some notifications were lost; the next sync must do
    /// a full local discovery. Orthogonal to [`FolderWatcher::is_reliable`].
    LostChanges,
    /// `becameUnreliable(message)`: the watcher cannot be trusted to capture
    /// all notifications any more. The message can be shown to users.
    BecameUnreliable(String),
}

/// The `FolderWatcher` members other than the platform part.
struct Core {
    events: UnboundedSender<FolderWatcherEvent>,
    weak: Weak<RefCell<Inner>>,
    /// `_timer`: started by `init`.
    timer: Option<Instant>,
    last_paths: HashSet<String>,
    is_reliable: bool,
    can_set_permissions: bool,
    should_watch_for_file_unlocking: bool,
    /// Path of the expected test notification.
    test_notification_path: String,
    unlocked_files: BTreeSet<String>,
    locked_files: BTreeSet<String>,
    /// `_lockChangeDebouncingTimer`: a restart bumps the generation, a
    /// pending timeout of an older generation does nothing.
    lock_timer_generation: u64,
}

struct Inner {
    core: Core,
    d: Option<FolderWatcherPrivate>,
}

/// `FolderWatcher`. See the module documentation.
pub struct FolderWatcher {
    inner: Rc<RefCell<Inner>>,
    reader: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for FolderWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("FolderWatcher")
            .field("d", &inner.d)
            .field("is_reliable", &inner.core.is_reliable)
            .finish()
    }
}

impl Drop for FolderWatcher {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

/// `Utility::fileNamesEqual`: the canonical paths are equal (case sensitive
/// on Linux). Only meaningful for existing paths.
fn file_names_equal(a: &str, b: &str) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => !a.as_os_str().is_empty() && a == b,
        _ => false,
    }
}

/// `appendSubPaths(dir, subPaths)`: every file and directory below `dir`
/// (`QDir::NoDotAndDotDot | QDir::Dirs | QDir::Files`: hidden entries are
/// left out, symlinks are followed), depth first.
fn append_sub_paths(dir: &str, sub_paths: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind_ok = fs::metadata(e.path()).is_ok_and(|m| m.is_dir() || m.is_file());
            (kind_ok && !name.starts_with('.')).then_some(name)
        })
        .collect();
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    let base = dir.trim_end_matches('/');
    for name in names {
        let path = format!("{base}/{name}");
        sub_paths.push(path.clone());
        if filesystem::is_dir(&path) {
            append_sub_paths(&path, sub_paths);
        }
    }
}

impl Core {
    fn emit(&self, event: FolderWatcherEvent) {
        // A closed channel means nobody listens any more, like a signal
        // without connections.
        let _ = self.events.send(event);
    }

    /// `pathIsIgnored(path)`: upstream v34.0.5 only ignores empty paths; the
    /// exclusion rules are applied by the folder (`Folder::slotWatchedPathChanged`).
    fn path_is_ignored(path: &str) -> bool {
        path.is_empty()
    }

    /// `changeDetected(const QString &path)`: a directory brings everything
    /// below it.
    fn change_detected_path(&mut self, path: &str) {
        let mut paths = vec![path.to_owned()];
        if filesystem::is_dir(path) {
            append_sub_paths(path, &mut paths);
        }
        self.change_detected_paths(paths);
    }

    /// `changeDetected(const QStringList &paths)`.
    fn change_detected_paths(&mut self, paths: Vec<String>) {
        // TODO (upstream): this shortcut doesn't look very reliable:
        //   - why is the timeout only 1 second?
        //   - what if there is more than one file being updated frequently?
        //   - why do we skip the file altogether instead of e.g. reducing the
        //     upload frequency?

        // Check if the same path was reported within the last second.
        let paths_set: HashSet<String> = paths.iter().cloned().collect();
        if paths_set == self.last_paths
            && self
                .timer
                .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            // the same path was reported within the last second. Skip.
            return;
        }
        self.last_paths = paths_set;
        self.timer = Some(Instant::now());

        // `QSet<QString> changedPaths`, kept in first-seen order.
        let mut changed_paths: Vec<String> = Vec::new();
        let mut seen = HashSet::new();

        for path in paths {
            if !self.test_notification_path.is_empty()
                && file_names_equal(&path, &self.test_notification_path)
            {
                self.test_notification_path.clear();
            }

            let lock_file_name_pattern = lock_file::file_path_lock_file_pattern_match(&path);
            let check_result =
                lock_file::lock_file_target_file_path(&path, &lock_file_name_pattern);
            if self.should_watch_for_file_unlocking {
                // Lock file has been deleted, file now unlocked
                if check_result.locking_type == FileLockingType::Unlocked
                    && !check_result.path.is_empty()
                {
                    self.locked_files.remove(&check_result.path);
                    self.unlocked_files.insert(check_result.path.clone());
                }
            }

            if check_result.locking_type == FileLockingType::Locked && !check_result.path.is_empty()
            {
                self.unlocked_files.remove(&check_result.path);
                self.locked_files.insert(check_result.path.clone());
            }

            // ------- handle ignores:
            if Self::path_is_ignored(&path) {
                continue;
            }

            if seen.insert(path.clone()) {
                changed_paths.push(path);
            }
        }

        if !self.locked_files.is_empty() || !self.unlocked_files.is_empty() {
            self.restart_lock_change_debouncing_timer();
        }

        for path in changed_paths {
            info!(target: LOG, "change on path {path}");
            self.emit(FolderWatcherEvent::PathChanged(path));
        }
    }

    /// `_lockChangeDebouncingTimer` (single shot) `stop()` + `start()`.
    fn restart_lock_change_debouncing_timer(&mut self) {
        self.lock_timer_generation += 1;
        let generation = self.lock_timer_generation;
        let weak = self.weak.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(LOCK_CHANGE_DEBOUNCING_TIMER_INTERVAL).await;
            if let Some(inner) = weak.upgrade() {
                let mut inner = inner.borrow_mut();
                if inner.core.lock_timer_generation == generation {
                    inner.core.lock_change_debouncing_timer_timed_out();
                }
            }
        });
    }

    /// `lockChangeDebouncingTimerTimedOut()`.
    fn lock_change_debouncing_timer_timed_out(&mut self) {
        if !self.unlocked_files.is_empty() {
            let unlocked = std::mem::take(&mut self.unlocked_files);
            self.emit(FolderWatcherEvent::FilesLockReleased(unlocked));
        }
        if !self.locked_files.is_empty() {
            let locked = std::mem::take(&mut self.locked_files);
            self.emit(FolderWatcherEvent::FilesLockImposed(locked.clone()));
            self.emit(FolderWatcherEvent::LockedFilesFound(locked));
        }
    }
}

impl WatcherParent for Core {
    fn change_detected(&mut self, path: &str) {
        self.change_detected_path(path);
    }

    fn path_is_ignored(&self, path: &str) -> bool {
        Self::path_is_ignored(path)
    }

    fn watches_exhausted(&mut self) {
        if self.is_reliable {
            self.is_reliable = false;
            self.emit(FolderWatcherEvent::BecameUnreliable(
                WATCHES_EXHAUSTED_MESSAGE.to_owned(),
            ));
        }
    }

    fn lost_changes(&mut self) {
        self.emit(FolderWatcherEvent::LostChanges);
    }
}

impl Inner {
    /// `startNotificationTestWhenReady()`.
    fn start_notification_test_when_ready(&mut self) {
        let weak = self.core.weak.clone();
        if !self.d.as_ref().is_some_and(|d| d.ready) {
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().start_notification_test_when_ready();
                }
            });
            return;
        }

        let path = self.core.test_notification_path.clone();
        if Path::new(&path).exists() {
            let mtime = filesystem::get_mod_time(&path);
            debug!(target: LOG, "setModTime {path} {}", mtime + 1);
            filesystem::set_mod_time(&path, mtime + 1);
        } else if let Err(e) = fs::OpenOptions::new().append(true).create(true).open(&path) {
            warn!(target: LOG, "Failed to create test file: {path} {e}");
            return;
        }
        filesystem::set_file_hidden(&path, true);

        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Some(inner) = weak.upgrade() {
                let mut inner = inner.borrow_mut();
                if !inner.core.test_notification_path.is_empty() {
                    inner.core.emit(FolderWatcherEvent::BecameUnreliable(
                        NO_TEST_NOTIFICATION_MESSAGE.to_owned(),
                    ));
                }
                inner.core.test_notification_path.clear();
            }
        });
    }
}

impl FolderWatcher {
    /// `FolderWatcher(folder)`: the signals are sent on `events`. Call
    /// [`Self::init`] to start watching.
    pub fn new(events: UnboundedSender<FolderWatcherEvent>) -> Self {
        let inner = Rc::new_cyclic(|weak| {
            RefCell::new(Inner {
                core: Core {
                    events,
                    weak: weak.clone(),
                    timer: None,
                    last_paths: HashSet::new(),
                    is_reliable: true,
                    can_set_permissions: true,
                    should_watch_for_file_unlocking: false,
                    test_notification_path: String::new(),
                    unlocked_files: BTreeSet::new(),
                    locked_files: BTreeSet::new(),
                    lock_timer_generation: 0,
                },
                d: None,
            })
        });
        Self {
            inner,
            reader: None,
        }
    }

    /// `init(root)`: watches `root` (the path of the root of the folder,
    /// without trailing `/`) and its sub directories. Must run inside a
    /// `LocalSet`.
    pub fn init(&mut self, root: &str) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let (sys, fd): (Box<dyn InotifySys>, Option<AsyncFd<Inotify>>) = match Inotify::init() {
            Ok(inotify) => {
                let sys = Box::new(LinuxInotify::new(inotify.watches()));
                match AsyncFd::new(inotify) {
                    Ok(fd) => (sys, Some(fd)),
                    Err(e) => {
                        warn!(target: LOG, "notify_init() failed: {e}");
                        (sys, None)
                    }
                }
            }
            Err(e) => {
                warn!(target: LOG, "notify_init() failed: {e}");
                (Box::new(NoInotify), None)
            }
        };
        self.init_with(sys, root);
        if let Some(fd) = fd {
            self.reader = Some(tokio::task::spawn_local(read_notifications(
                fd,
                Rc::downgrade(&self.inner),
            )));
        }
    }

    /// [`Self::init`] with another implementation of the inotify calls
    /// (for tests); nothing reads events, feed them with
    /// [`Self::handle_inotify_events`].
    pub fn init_with(&mut self, sys: Box<dyn InotifySys>, root: &str) {
        let mut inner = self.inner.borrow_mut();
        let d = FolderWatcherPrivate::new(sys, root, &mut inner.core);
        inner.d = Some(d);
        inner.core.timer = Some(Instant::now());
    }

    /// Handles decoded inotify events as if read from the descriptor.
    pub fn handle_inotify_events(&self, events: &[linux::RawInotifyEvent]) {
        let mut inner = self.inner.borrow_mut();
        let Inner { core, d } = &mut *inner;
        if let Some(d) = d {
            d.handle_events(events, core);
        }
    }

    /// `isReliable()`: false if the folder watcher can't be trusted to
    /// capture all notifications, for example when the inotify user limit
    /// from `/proc/sys/fs/inotify/max_user_watches` is exceeded.
    pub fn is_reliable(&self) -> bool {
        self.inner.borrow().core.is_reliable
    }

    /// `canSetPermissions()`.
    pub fn can_set_permissions(&self) -> bool {
        self.inner.borrow().core.can_set_permissions
    }

    /// `startNotificatonTest(path)`: changes `path` and checks that a
    /// notification arrives within 5 seconds; otherwise sends
    /// [`FolderWatcherEvent::BecameUnreliable`]. The path must be ignored by
    /// the folder (upstream uses `<root>.nextcloudsync.log`).
    pub fn start_notificaton_test(&self, path: &str) {
        let mut inner = self.inner.borrow_mut();
        debug_assert!(inner.core.test_notification_path.is_empty());
        inner.core.test_notification_path = path.to_owned();

        // Don't do the local file modification immediately:
        // wait for FolderWatchPrivate::_ready
        inner.start_notification_test_when_ready();
    }

    /// `performSetPermissionsTest(path)`: tries to make a file read-only; if
    /// that fails, [`Self::can_set_permissions`] becomes false.
    pub fn perform_set_permissions_test(&self, path: &str) {
        let mut inner = self.inner.borrow_mut();
        let core = &mut inner.core;
        core.can_set_permissions = true;

        if !Path::new(path).exists()
            && let Err(e) = fs::write(path, "test")
        {
            warn!(target: LOG, "Failed to create test file: {path} {e}");
            return;
        }

        if !filesystem::is_writable(path) {
            filesystem::set_file_read_only(path, false);
            if !filesystem::is_writable(path) {
                warn!(target: LOG, "Cannot make file readable: {path}");
                core.can_set_permissions = false;
                let _ = fs::remove_file(path);
                return;
            }
        }

        filesystem::set_file_read_only(path, true);
        if filesystem::is_writable(path) {
            warn!(target: LOG, "Cannot make file read-only: {path}");
            core.can_set_permissions = false;
        }

        info!(
            target: LOG,
            "Permissions in file system for {path} {}",
            if core.can_set_permissions { "works as expected" } else { "are not reliable" }
        );

        filesystem::set_file_read_only(path, false);
        let _ = fs::remove_file(path);
    }

    /// `testLinuxWatchCount()`: the number of inotify watches (0 before
    /// [`Self::init`]).
    pub fn test_linux_watch_count(&self) -> usize {
        self.inner
            .borrow()
            .d
            .as_ref()
            .map_or(0, FolderWatcherPrivate::test_watch_count)
    }

    /// `slotLockFileDetectedExternally(lockFile)`: the sync engine found a
    /// lock file (`SyncEngine::lockFileDetected`), probably a newly-uploaded
    /// office file.
    pub fn slot_lock_file_detected_externally(&self, lock_file: &str) {
        info!(target: LOG, "Lock file detected externally, probably a newly-uploaded office file: {lock_file}");
        self.inner.borrow_mut().core.change_detected_path(lock_file);
    }

    /// `setShouldWatchForFileUnlocking(...)`.
    pub fn set_should_watch_for_file_unlocking(&self, should_watch_for_file_unlocking: bool) {
        self.inner.borrow_mut().core.should_watch_for_file_unlocking =
            should_watch_for_file_unlocking;
    }

    /// `lockChangeDebouncingTimout()`: the delay after the last lock change
    /// before `filesLockReleased` / `filesLockImposed` are sent.
    pub fn lock_change_debouncing_timout(&self) -> Duration {
        LOCK_CHANGE_DEBOUNCING_TIMER_INTERVAL
    }

    /// `folderAccountCapabilitiesChanged()`: upstream reads
    /// `capabilities().filesLockAvailable()` from the folder's account, on
    /// construction and on `Account::capabilitiesChanged`.
    pub fn folder_account_capabilities_changed(&self, files_lock_available: bool) {
        self.set_should_watch_for_file_unlocking(files_lock_available);
    }
}

/// The `QSocketNotifier` of `FolderWatcherPrivate`: calls
/// `slotReceivedNotification` while the descriptor is readable.
async fn read_notifications(fd: AsyncFd<Inotify>, weak: Weak<RefCell<Inner>>) {
    loop {
        let Ok(mut guard) = fd.readable().await else {
            break;
        };
        let Some(inner) = weak.upgrade() else {
            break;
        };
        let result = {
            let mut inner = inner.borrow_mut();
            let Inner { core, d } = &mut *inner;
            match d {
                Some(d) => d.slot_received_notification(fd.get_ref().as_fd(), core),
                None => Ok(()),
            }
        };
        drop(inner);
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => guard.clear_ready(),
            Err(e) => {
                warn!(target: LOG, "Reading inotify events failed: {e}");
                break;
            }
        }
    }
}
