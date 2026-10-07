/*
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2011 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testfolderwatcher.cpp (nextcloud/desktop v34.0.5).
//
// Upstream runs every test function in order on one fixture (one tree, one
// watcher). Rust tests are independent: each builds the fixture, runs
// upstream's `init()` check, its body and the `cleanup()` check. A test that
// relied on an earlier one first replays that step (noted).
//
// The `QSignalSpy`s are the events received on the watcher's channel; like
// Qt's spies they only see what the event loop delivered while waiting.

use std::fs;
use std::future::Future;
use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

use inotify::{EventMask, WatchMask};
use nc_daemon::folder_watcher::linux::{InotifySys, RawInotifyEvent};
use nc_daemon::folder_watcher::{
    FolderWatcher, FolderWatcherEvent, NO_TEST_NOTIFICATION_MESSAGE, WATCHES_EXHAUSTED_MESSAGE,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn run_shell(program: &str, args: &[&str]) {
    eprintln!("Command: {program} {}", args.join(" "));
    let _ = Command::new(program).args(args).status();
}

fn touch(file: &str) {
    run_shell("touch", &[file]);
}

fn mkdir(file: &str) {
    run_shell("mkdir", &[file]);
}

fn rmdir(file: &str) {
    run_shell("rmdir", &[file]);
}

fn rm(file: &str) {
    run_shell("rm", &[file]);
}

fn mv(file1: &str, file2: &str) {
    run_shell("mv", &[file1, file2]);
}

/// `Utility::writeRandomFile(fname)`.
fn write_random_file(path: &str) {
    let size = 10 + (std::process::id() as usize % 200);
    let content: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();
    fs::write(path, content).unwrap();
}

fn exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

/// Runs a test body in a `LocalSet`, like the Qt event loop of the test.
async fn local<F: Future<Output = ()>>(body: F) {
    tokio::task::LocalSet::new().run_until(body).await;
}

struct TestFolderWatcher {
    _root: tempfile::TempDir,
    root_path: String,
    watcher: FolderWatcher,
    rx: UnboundedReceiver<FolderWatcherEvent>,
    /// Every event received since the last `clear`.
    spy: Vec<FolderWatcherEvent>,
}

impl TestFolderWatcher {
    /// The constructor of upstream's test class.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path = fs::canonicalize(root.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        eprintln!("creating test directory tree in {root_path}");

        for p in ["a1/b1/c1", "a1/b1/c2", "a1/b2/c1", "a1/b3/c3", "a2/b3/c3"] {
            fs::create_dir_all(format!("{root_path}/{p}")).unwrap();
        }
        write_random_file(&format!("{root_path}/a1/random.bin"));
        write_random_file(&format!("{root_path}/a1/b2/todelete.bin"));
        write_random_file(&format!("{root_path}/a2/renamefile"));
        write_random_file(&format!("{root_path}/a1/movefile"));

        let (tx, rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init(&root_path);
        Self {
            _root: root,
            root_path,
            watcher,
            rx,
            spy: Vec::new(),
        }
    }

    /// `_watcher.reset(new FolderWatcher); _watcher->init(_rootPath);` with
    /// a new spy.
    fn reset_watcher(&mut self) {
        let (tx, rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init(&self.root_path);
        self.watcher = watcher;
        self.rx = rx;
        self.spy.clear();
    }

    fn count_folders(path: &str) -> usize {
        let mut n = 0;
        for entry in fs::read_dir(path).unwrap().filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && entry.path().is_dir() {
                n += 1 + Self::count_folders(&format!("{path}/{name}"));
            }
        }
        n
    }

    fn check_watch_count(&self) {
        assert_eq!(
            self.watcher.test_linux_watch_count(),
            Self::count_folders(&self.root_path) + 1
        );
    }

    /// `init()`.
    fn init(&mut self) {
        self.spy.clear();
        self.check_watch_count();
    }

    /// `cleanup()`.
    fn cleanup(&self) {
        self.check_watch_count();
    }

    /// Waits up to `timeout` for the next event and records it
    /// (`QSignalSpy::wait`). Returns whether one arrived.
    async fn wait(&mut self, timeout: Duration) -> bool {
        match tokio::time::timeout(timeout, self.rx.recv()).await {
            Ok(Some(event)) => {
                self.spy.push(event);
                true
            }
            _ => false,
        }
    }

    fn path_changed_reported(&self, path: &str) -> bool {
        self.spy
            .iter()
            .any(|e| matches!(e, FolderWatcherEvent::PathChanged(p) if p == path))
    }

    /// `waitForPathChanged(path)`.
    async fn wait_for_path_changed(&mut self, path: &str) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(5000) {
            // Check if it was already reported as changed by the watcher
            if self.path_changed_reported(path) {
                return true;
            }
            // Wait a bit and test again (don't bother checking if we timed out or not)
            self.wait(Duration::from_millis(200)).await;
        }
        false
    }

    /// A `QSignalSpy` created now: the index of the first event it sees.
    fn spy_start(&self) -> usize {
        self.spy.len()
    }

    /// The emissions matching `filter` since `start`.
    fn spied<T>(&self, start: usize, filter: impl Fn(&FolderWatcherEvent) -> Option<T>) -> Vec<T> {
        self.spy[start..].iter().filter_map(filter).collect()
    }

    /// `spy->wait(timeout)` for one signal: returns once a matching event
    /// arrives after `start`, or after `timeout`.
    async fn wait_for_signal(
        &mut self,
        start: usize,
        timeout: Duration,
        filter: fn(&FolderWatcherEvent) -> bool,
    ) {
        let t = Instant::now();
        let already = self.spy[start..].iter().filter(|e| filter(e)).count();
        while t.elapsed() < timeout {
            if self.spy[start..].iter().filter(|e| filter(e)).count() > already {
                return;
            }
            let left = timeout.saturating_sub(t.elapsed());
            if !self.wait(left).await {
                return;
            }
        }
    }
}

fn locks_imposed(e: &FolderWatcherEvent) -> Option<Vec<String>> {
    match e {
        FolderWatcherEvent::FilesLockImposed(files) => Some(files.iter().cloned().collect()),
        _ => None,
    }
}

fn locks_released(e: &FolderWatcherEvent) -> Option<Vec<String>> {
    match e {
        FolderWatcherEvent::FilesLockReleased(files) => Some(files.iter().cloned().collect()),
        _ => None,
    }
}

#[tokio::test]
async fn test_a_create() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        // create a new file
        let file = format!("{}/foo.txt", t.root_path);
        run_shell("sh", &["-c", &format!("echo \"xyz\" > \"{file}\"")]);

        assert!(t.wait_for_path_changed(&file).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_a_touch() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        // touch an existing file.
        let file = format!("{}/a1/random.bin", t.root_path);
        touch(&file);
        assert!(t.wait_for_path_changed(&file).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_move3_level_dir_with_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let root = t.root_path.clone();
        let file = format!("{root}/a0/b/c/empty.txt");
        mkdir(&format!("{root}/a0"));
        mkdir(&format!("{root}/a0/b"));
        mkdir(&format!("{root}/a0/b/c"));
        touch(&file);
        mv(&format!("{root}/a0"), &format!("{root}/a"));
        assert!(
            t.wait_for_path_changed(&format!("{root}/a/b/c/empty.txt"))
                .await
        );
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_create_a_dir() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let file = format!("{}/a1/b1/new_dir", t.root_path);
        mkdir(&file);
        assert!(t.wait_for_path_changed(&file).await);

        // Notifications from that new folder arrive too
        let file2 = format!("{}/a1/b1/new_dir/contained", t.root_path);
        touch(&file2);
        assert!(t.wait_for_path_changed(&file2).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_remove_a_dir() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let file = format!("{}/a1/b3/c3", t.root_path);
        rmdir(&file);
        assert!(t.wait_for_path_changed(&file).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_remove_a_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let file = format!("{}/a1/b2/todelete.bin", t.root_path);
        assert!(exists(&file));
        rm(&file);
        assert!(!exists(&file));

        assert!(t.wait_for_path_changed(&file).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_rename_a_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let file1 = format!("{}/a2/renamefile", t.root_path);
        let file2 = format!("{}/a2/renamefile.renamed", t.root_path);
        assert!(exists(&file1));
        mv(&file1, &file2);
        assert!(exists(&file2));

        assert!(t.wait_for_path_changed(&file1).await);
        assert!(t.wait_for_path_changed(&file2).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_move_a_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let old_file = format!("{}/a1/movefile", t.root_path);
        let new_file = format!("{}/a2/movefile.renamed", t.root_path);
        assert!(exists(&old_file));
        mv(&old_file, &new_file);
        assert!(exists(&new_file));

        assert!(t.wait_for_path_changed(&old_file).await);
        assert!(t.wait_for_path_changed(&new_file).await);
        t.cleanup();
    })
    .await;
}

/// The body of `testRenameDirectorySameBase`, which
/// `testRenameDirectoryDifferentBase` builds on.
async fn rename_directory_same_base(t: &mut TestFolderWatcher) {
    let old_file = format!("{}/a1/b1", t.root_path);
    let new_file = format!("{}/a1/brename", t.root_path);
    assert!(exists(&old_file));
    mv(&old_file, &new_file);
    assert!(exists(&new_file));

    assert!(t.wait_for_path_changed(&old_file).await);
    assert!(t.wait_for_path_changed(&new_file).await);

    // Verify that further notifications end up with the correct paths

    let file = format!("{}/a1/brename/c1/random.bin", t.root_path);
    touch(&file);
    assert!(t.wait_for_path_changed(&file).await);

    let dir = format!("{}/a1/brename/newfolder", t.root_path);
    mkdir(&dir);
    assert!(t.wait_for_path_changed(&dir).await);
}

#[tokio::test]
async fn test_rename_directory_same_base() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        rename_directory_same_base(&mut t).await;
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_rename_directory_different_base() {
    local(async {
        let mut t = TestFolderWatcher::new();
        // Upstream runs after testRenameDirectorySameBase, which created
        // a1/brename (with a1/brename/newfolder).
        rename_directory_same_base(&mut t).await;
        t.init();

        let old_file = format!("{}/a1/brename", t.root_path);
        let new_file = format!("{}/bren", t.root_path);
        assert!(exists(&old_file));
        mv(&old_file, &new_file);
        assert!(exists(&new_file));

        assert!(t.wait_for_path_changed(&old_file).await);
        assert!(t.wait_for_path_changed(&new_file).await);

        // Verify that further notifications end up with the correct paths

        let file = format!("{}/bren/c1/random.bin", t.root_path);
        touch(&file);
        assert!(t.wait_for_path_changed(&file).await);

        let dir = format!("{}/bren/newfolder2", t.root_path);
        mkdir(&dir);
        assert!(t.wait_for_path_changed(&dir).await);
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_detect_lock_files() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let root = t.root_path.clone();
        let mut list_of_office_files = vec![
            format!("{root}/document.docx"),
            format!("{root}/document.odt"),
        ];
        list_of_office_files.sort();

        let list_of_office_lock_files = [
            format!("{root}/.~lock.document.docx#"),
            format!("{root}/.~lock.document.odt#"),
        ];

        t.watcher.set_should_watch_for_file_unlocking(true);

        // verify that office files for locking got detected by the watcher
        let locks_imposed_spy = t.spy_start();

        for office_file in &list_of_office_files {
            touch(office_file);
            assert!(t.wait_for_path_changed(office_file).await);
        }
        for office_lock_file in &list_of_office_lock_files {
            touch(office_lock_file);
            assert!(t.wait_for_path_changed(office_lock_file).await);
        }

        let timeout = t.watcher.lock_change_debouncing_timout() + Duration::from_millis(100);
        t.wait_for_signal(locks_imposed_spy, timeout, |e| locks_imposed(e).is_some())
            .await;
        let imposed = t.spied(locks_imposed_spy, locks_imposed);
        assert_eq!(imposed.len(), 1);
        let mut locked_office_files_by_watcher = imposed[0].clone();
        locked_office_files_by_watcher.sort();
        assert_eq!(
            list_of_office_files.len(),
            locked_office_files_by_watcher.len()
        );

        for i in 0..list_of_office_files.len() {
            assert!(list_of_office_files[i] == locked_office_files_by_watcher[i]);
        }

        // verify that office files for unlocking got detected by the watcher
        let locks_released_spy = t.spy_start();

        for office_lock_file in &list_of_office_lock_files {
            rm(office_lock_file);
            assert!(t.wait_for_path_changed(office_lock_file).await);
        }

        t.wait_for_signal(locks_released_spy, timeout, |e| locks_released(e).is_some())
            .await;
        let released = t.spied(locks_released_spy, locks_released);
        assert_eq!(released.len(), 1);
        let mut unlocked_office_files_by_watcher = released[0].clone();
        unlocked_office_files_by_watcher.sort();
        assert_eq!(
            list_of_office_files.len(),
            unlocked_office_files_by_watcher.len()
        );

        for i in 0..list_of_office_files.len() {
            assert!(list_of_office_files[i] == unlocked_office_files_by_watcher[i]);
        }

        // cleanup
        for office_lock_file in &list_of_office_lock_files {
            rm(office_lock_file);
        }
        for office_file in &list_of_office_files {
            rm(office_file);
        }
        t.cleanup();
    })
    .await;
}

#[tokio::test]
async fn test_detect_lock_files_externally() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init();
        let root = t.root_path.clone();
        let mut list_of_office_files = vec![
            format!("{root}/document.docx"),
            format!("{root}/document.odt"),
        ];
        list_of_office_files.sort();

        let list_of_office_lock_files = [
            format!("{root}/.~lock.document.docx#"),
            format!("{root}/.~lock.document.odt#"),
        ];

        for office_file in &list_of_office_files {
            touch(office_file);
        }
        for office_lock_file in &list_of_office_lock_files {
            touch(office_lock_file);
        }

        t.reset_watcher();
        t.watcher.set_should_watch_for_file_unlocking(true);
        let locks_imposed_spy = t.spy_start();
        let locks_released_spy = t.spy_start();

        for office_lock_file in &list_of_office_lock_files {
            t.watcher
                .slot_lock_file_detected_externally(office_lock_file);
        }

        // locked files detected
        let timeout = t.watcher.lock_change_debouncing_timout() + Duration::from_millis(100);
        t.wait_for_signal(locks_imposed_spy, timeout, |e| locks_imposed(e).is_some())
            .await;
        let imposed = t.spied(locks_imposed_spy, locks_imposed);
        assert_eq!(imposed.len(), 1);
        let mut locked_office_files_by_watcher = imposed[0].clone();
        locked_office_files_by_watcher.sort();
        assert_eq!(
            list_of_office_files.len(),
            locked_office_files_by_watcher.len()
        );
        for i in 0..list_of_office_files.len() {
            assert!(list_of_office_files[i] == locked_office_files_by_watcher[i]);
        }

        // unlocked files detected
        for office_lock_file in &list_of_office_lock_files {
            rm(office_lock_file);
        }
        t.wait_for_signal(locks_released_spy, timeout, |e| locks_released(e).is_some())
            .await;
        let released = t.spied(locks_released_spy, locks_released);
        assert_eq!(released.len(), 1);
        let mut unlocked_office_files_by_watcher = released[0].clone();
        unlocked_office_files_by_watcher.sort();
        assert_eq!(
            list_of_office_files.len(),
            unlocked_office_files_by_watcher.len()
        );

        for i in 0..list_of_office_files.len() {
            assert!(list_of_office_files[i] == unlocked_office_files_by_watcher[i]);
        }

        // cleanup
        for office_file in &list_of_office_files {
            rm(office_file);
        }
        for office_lock_file in &list_of_office_lock_files {
            rm(office_lock_file);
        }
        t.cleanup();
    })
    .await;
}

// ---------------------------------------------------------------------------
// Derived tests (no upstream counterpart): the Linux paths upstream does not
// test, through the `InotifySys` seam.

/// Every `inotify_add_watch` fails with `ENOSPC` (watches exhausted).
struct ExhaustedInotify;

impl InotifySys for ExhaustedInotify {
    fn add_watch(&mut self, _path: &str, _mask: WatchMask) -> io::Result<i32> {
        Err(io::Error::from_raw_os_error(28)) // ENOSPC
    }
    fn rm_watch(&mut self, _wd: i32) {}
}

/// Hands out watch descriptors and records the removals.
#[derive(Default)]
struct FakeInotify {
    next: i32,
}

impl InotifySys for FakeInotify {
    fn add_watch(&mut self, _path: &str, mask: WatchMask) -> io::Result<i32> {
        assert_eq!(
            mask.bits(),
            WatchMask::CLOSE_WRITE.bits()
                | WatchMask::ATTRIB.bits()
                | WatchMask::MOVE.bits()
                | WatchMask::CREATE.bits()
                | WatchMask::DELETE.bits()
                | WatchMask::DELETE_SELF.bits()
                | WatchMask::MOVE_SELF.bits()
                | EventMask::UNMOUNT.bits()
                | WatchMask::ONLYDIR.bits()
        );
        self.next += 1;
        Ok(self.next)
    }
    fn rm_watch(&mut self, _wd: i32) {}
}

#[tokio::test]
async fn derived_watches_exhausted_makes_unreliable_once() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/b")).unwrap();
        let (tx, mut rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init_with(Box::new(ExhaustedInotify), root.path().to_str().unwrap());
        assert!(!watcher.is_reliable());
        assert_eq!(watcher.test_linux_watch_count(), 0);
        assert_eq!(
            rx.try_recv().unwrap(),
            FolderWatcherEvent::BecameUnreliable(WATCHES_EXHAUSTED_MESSAGE.to_owned())
        );
        assert!(rx.try_recv().is_err());
    })
    .await;
}

#[tokio::test]
async fn derived_queue_overflow_reports_lost_changes() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (tx, mut rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init_with(Box::<FakeInotify>::default(), root.path().to_str().unwrap());
        watcher.handle_inotify_events(&[RawInotifyEvent {
            wd: -1,
            mask: EventMask::Q_OVERFLOW.bits(),
            cookie: 0,
            len: 0,
            name: Vec::new(),
        }]);
        assert_eq!(rx.try_recv().unwrap(), FolderWatcherEvent::LostChanges);
    })
    .await;
}

#[tokio::test]
async fn derived_journal_files_and_unknown_descriptors_are_filtered() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_str().unwrap().to_owned();
        let (tx, mut rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init_with(Box::<FakeInotify>::default(), &root_path);
        let event = |wd: i32, name: &str| RawInotifyEvent {
            wd,
            mask: EventMask::CLOSE_WRITE.bits(),
            cookie: 0,
            len: 16,
            name: name.as_bytes().to_vec(),
        };
        watcher.handle_inotify_events(&[
            event(1, ".sync_0123456789ab.db"),
            event(1, ".sync_0123456789ab.db-wal"),
            event(1, "._sync_old.db"),
            event(1, ".csync_journal.db"),
            event(7, "elsewhere"),
            event(1, "file.txt"),
        ]);
        assert_eq!(
            rx.try_recv().unwrap(),
            FolderWatcherEvent::PathChanged(format!("{root_path}/file.txt"))
        );
        assert!(rx.try_recv().is_err());
    })
    .await;
}

#[tokio::test]
async fn derived_notification_test() {
    local(async {
        // A real watcher sees the test file change: no unreliability.
        let root = tempfile::tempdir().unwrap();
        let root_path = fs::canonicalize(root.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (tx, mut rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init(&root_path);
        let test_path = format!("{root_path}/.nextcloudsync.log");
        watcher.start_notificaton_test(&test_path);
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(5500);
        while let Ok(Some(e)) = tokio::time::timeout(deadline - Instant::now(), rx.recv()).await {
            seen.push(e);
        }
        assert!(seen.contains(&FolderWatcherEvent::PathChanged(test_path.clone())));
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, FolderWatcherEvent::BecameUnreliable(_)))
        );
        assert!(watcher.is_reliable());

        // Without notifications (fake descriptor), the watcher complains
        // after 5 s, and stays "reliable" like upstream.
        let (tx, mut rx) = unbounded_channel();
        let mut watcher = FolderWatcher::new(tx);
        watcher.init_with(Box::<FakeInotify>::default(), &root_path);
        watcher.start_notificaton_test(&test_path);
        let e = tokio::time::timeout(Duration::from_millis(5500), rx.recv())
            .await
            .unwrap();
        assert_eq!(
            e,
            Some(FolderWatcherEvent::BecameUnreliable(
                NO_TEST_NOTIFICATION_MESSAGE.to_owned()
            ))
        );
        assert!(watcher.is_reliable());
    })
    .await;
}

#[tokio::test]
async fn derived_set_permissions_test() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = unbounded_channel();
        let watcher = FolderWatcher::new(tx);
        let path = format!(
            "{}/.nextcloudpermissions.log",
            root.path().to_str().unwrap()
        );
        watcher.perform_set_permissions_test(&path);
        // Root can write read-only files, so the test fails as root.
        let is_root = rustix::process::geteuid().is_root();
        assert_eq!(watcher.can_set_permissions(), !is_root);
        assert!(!exists(&path));
    })
    .await;
}
