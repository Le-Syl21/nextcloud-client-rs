// Upstream gates nothing here: `testfolderwatcher.cpp` runs on every system.
// The Linux port (`testfolderwatcher.rs`) drives inotify; this one runs the
// same scenarios against the Windows backend (`ReadDirectoryChangesW`) and the
// macOS one (FSEvents, created with `IgnoreSelf`: the file operations run as
// child processes, like upstream's `system()` calls).
#![cfg(any(windows, target_os = "macos"))]

/*
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2011 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testfolderwatcher.cpp (nextcloud/desktop v34.0.5),
// Windows helpers included (the file operations use the standard library
// where upstream runs `cmd /c`). Like the Linux port, every test builds the
// fixture, runs upstream's `init()` and its body; a test that relied on an
// earlier one replays that step.
//
// Not ported here: the lock file tests (`testDetectLockFiles`, run by the
// Linux port against the shared change detection) and `CHECK_WATCH_COUNT`,
// which upstream also keeps Linux-only.

use std::fs;
use std::future::Future;
use std::time::{Duration, Instant};

use nc_daemon::folder_watcher::{FolderWatcher, FolderWatcherEvent};
use nc_platform::paths::from_long_win_path;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Runs `program args` (upstream's `system()`); on macOS the watcher ignores
/// this process's own changes, so every file operation goes through a child.
fn run(program: &str, args: &[&str]) {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{program} {args:?}");
}

fn touch(file: &str) {
    if cfg!(target_os = "macos") {
        run("touch", &[file]);
        return;
    }
    // Like `copy /b file +,,`: create, or bump the mtime.
    let f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    f.set_modified(std::time::SystemTime::now()).unwrap();
}

fn mkdir(file: &str) {
    if cfg!(target_os = "macos") {
        run("mkdir", &[file]);
    } else {
        fs::create_dir(file).unwrap();
    }
}

fn mv(from: &str, to: &str) {
    if cfg!(target_os = "macos") {
        run("mv", &[from, to]);
    } else {
        fs::rename(from, to).unwrap();
    }
}

fn rm(file: &str) {
    if cfg!(target_os = "macos") {
        run("rm", &[file]);
    } else {
        fs::remove_file(file).unwrap();
    }
}

fn rmdir(file: &str) {
    if cfg!(target_os = "macos") {
        run("rmdir", &[file]);
    } else {
        fs::remove_dir(file).unwrap();
    }
}

fn write_file(file: &str, content: &str) {
    if cfg!(target_os = "macos") {
        run(
            "sh",
            &["-c", &format!("printf '%s' '{content}' > '{file}'")],
        );
    } else {
        fs::write(file, content).unwrap();
    }
}

fn write_random_file(path: &str) {
    let size = 10 + (std::process::id() as usize % 200);
    let content: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();
    fs::write(path, content).unwrap();
}

fn exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

async fn local<F: Future<Output = ()>>(body: F) {
    tokio::task::LocalSet::new().run_until(body).await;
}

struct TestFolderWatcher {
    _root: tempfile::TempDir,
    root_path: String,
    _watcher: FolderWatcher,
    rx: UnboundedReceiver<FolderWatcherEvent>,
    spy: Vec<FolderWatcherEvent>,
}

impl TestFolderWatcher {
    /// The constructor of upstream's test class.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path =
            from_long_win_path(&fs::canonicalize(root.path()).unwrap().to_string_lossy());
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
            _watcher: watcher,
            rx,
            spy: Vec::new(),
        }
    }

    /// `init()`: the watcher is armed; what the fixture itself did is not
    /// reported to the test.
    async fn init(&mut self) {
        // Let the thread arm its first request, then forget what the
        // creation of the tree reported.
        tokio::time::sleep(Duration::from_millis(500)).await;
        while self.rx.try_recv().is_ok() {}
        self.spy.clear();
    }

    async fn wait(&mut self, timeout: Duration) -> bool {
        match tokio::time::timeout(timeout, self.rx.recv()).await {
            Ok(Some(event)) => {
                self.spy.push(event);
                true
            }
            _ => false,
        }
    }

    /// `waitForPathChanged(path)`.
    async fn wait_for_path_changed(&mut self, path: &str) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(5000) {
            if self
                .spy
                .iter()
                .any(|e| matches!(e, FolderWatcherEvent::PathChanged(p) if p == path))
            {
                return true;
            }
            self.wait(Duration::from_millis(200)).await;
        }
        eprintln!("not reported: {path}; got {:?}", self.spy);
        false
    }
}

#[tokio::test]
async fn test_a_create() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file = format!("{}/foo.txt", t.root_path);
        write_file(&file, "xyz");
        assert!(t.wait_for_path_changed(&file).await);
    })
    .await;
}

#[tokio::test]
async fn test_a_touch() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file = format!("{}/a1/random.bin", t.root_path);
        touch(&file);
        assert!(t.wait_for_path_changed(&file).await);
    })
    .await;
}

#[tokio::test]
async fn test_move3_level_dir_with_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
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
    })
    .await;
}

#[tokio::test]
async fn test_create_a_dir() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file = format!("{}/a1/b1/new_dir", t.root_path);
        mkdir(&file);
        assert!(t.wait_for_path_changed(&file).await);

        // Notifications from that new folder arrive too
        let file2 = format!("{}/a1/b1/new_dir/contained", t.root_path);
        touch(&file2);
        assert!(t.wait_for_path_changed(&file2).await);
    })
    .await;
}

#[tokio::test]
async fn test_remove_a_dir() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file = format!("{}/a1/b3/c3", t.root_path);
        rmdir(&file);
        assert!(t.wait_for_path_changed(&file).await);
    })
    .await;
}

#[tokio::test]
async fn test_remove_a_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file = format!("{}/a1/b2/todelete.bin", t.root_path);
        assert!(exists(&file));
        rm(&file);
        assert!(!exists(&file));
        assert!(t.wait_for_path_changed(&file).await);
    })
    .await;
}

#[tokio::test]
async fn test_rename_a_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file1 = format!("{}/a2/renamefile", t.root_path);
        let file2 = format!("{}/a2/renamefile.renamed", t.root_path);
        mv(&file1, &file2);
        assert!(t.wait_for_path_changed(&file1).await);
        assert!(t.wait_for_path_changed(&file2).await);
    })
    .await;
}

#[tokio::test]
async fn test_move_a_file() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let old_file = format!("{}/a1/movefile", t.root_path);
        let new_file = format!("{}/a2/movefile.renamed", t.root_path);
        mv(&old_file, &new_file);
        assert!(t.wait_for_path_changed(&old_file).await);
        assert!(t.wait_for_path_changed(&new_file).await);
    })
    .await;
}

#[tokio::test]
async fn test_rename_directory_same_and_different_base() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let root = t.root_path.clone();
        // testRenameDirectorySameBase
        let (old_dir, new_dir) = (format!("{root}/a1/b1"), format!("{root}/a1/brename"));
        mv(&old_dir, &new_dir);
        assert!(t.wait_for_path_changed(&old_dir).await);
        assert!(t.wait_for_path_changed(&new_dir).await);
        // Verify that further notifications end up with the correct paths
        let file = format!("{root}/a1/brename/c1/random.bin");
        touch(&file);
        assert!(t.wait_for_path_changed(&file).await);
        let dir = format!("{root}/a1/brename/newfolder");
        mkdir(&dir);
        assert!(t.wait_for_path_changed(&dir).await);

        // testRenameDirectoryDifferentBase
        t.init().await;
        let (old_dir, new_dir) = (format!("{root}/a1/brename"), format!("{root}/bren"));
        mv(&old_dir, &new_dir);
        assert!(t.wait_for_path_changed(&old_dir).await);
        assert!(t.wait_for_path_changed(&new_dir).await);
        let file = format!("{root}/bren/c1/random.bin");
        touch(&file);
        assert!(t.wait_for_path_changed(&file).await);
        let dir = format!("{root}/bren/newfolder2");
        mkdir(&dir);
        assert!(t.wait_for_path_changed(&dir).await);
    })
    .await;
}

#[cfg(windows)]
/// A trailing dot or space is kept by the verbatim paths of the watcher
/// (the reported name is the one the file system has).
#[tokio::test]
async fn derived_name_with_trailing_dot() {
    local(async {
        let mut t = TestFolderWatcher::new();
        t.init().await;
        let file = format!("{}/trailing.", t.root_path);
        fs::write(nc_platform::paths::long_win_path(&file), "x").unwrap();
        assert!(t.wait_for_path_changed(&file).await);
    })
    .await;
}
