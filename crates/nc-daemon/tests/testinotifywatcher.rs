/*
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testinotifywatcher.cpp (nextcloud/desktop v34.0.5).
//
// Upstream subclasses `FolderWatcherPrivate` (default-constructed: no
// descriptor, no parent) to reach its protected members; the port uses the
// public `FolderWatcherPrivate::default()` and the null parent `()`.

use std::fs;
use std::io::Write;
use std::os::fd::AsFd;

use inotify::EventMask;
use nc_daemon::folder_watcher::linux::{FolderWatcherPrivate, INOTIFY_EVENT_SIZE};

/// `initTestCase()`: the directory tree, in a fresh temporary directory
/// (removed on drop, which is `cleanupTestCase()`).
fn init_test_case() -> (tempfile::TempDir, String) {
    let dir = tempfile::Builder::new().prefix("test_").tempdir().unwrap();
    let root = dir.path().to_str().unwrap().to_owned();
    for p in ["a1/b1/c1", "a1/b1/c2", "a1/b2/c1", "a1/b3/c3", "a2/b3/c3"] {
        fs::create_dir_all(format!("{root}/{p}")).unwrap();
    }
    (dir, root)
}

/// `Utility::writeRandomFile(fname)`.
fn write_random_file(path: &str) -> bool {
    let size = 10 + (std::process::id() as usize % 200);
    let content: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();
    fs::write(path, content).is_ok()
}

/// `QStringList::indexOf`: -1 when missing.
fn index_of(list: &[String], value: &str) -> i64 {
    list.iter()
        .position(|v| v == value)
        .map_or(-1, |i| i as i64)
}

// Test the recursive path listing function findFoldersBelow
#[test]
fn test_dirs_below_path() {
    let (_dir, root) = init_test_case();
    let watcher = FolderWatcherPrivate::default();
    let mut dirs = Vec::new();

    let ok = watcher.find_folders_below(&root, &mut dirs);
    assert!(index_of(&dirs, &format!("{root}/a1")) > -1);
    assert!(index_of(&dirs, &format!("{root}/a1/b1")) > -1);
    assert!(index_of(&dirs, &format!("{root}/a1/b1/c1")) > -1);
    assert!(index_of(&dirs, &format!("{root}/a1/b1/c2")) > -1);

    assert!(write_random_file(&format!("{root}/a1/rand1.dat")));
    assert!(write_random_file(&format!("{root}/a1/b1/rand2.dat")));
    assert!(write_random_file(&format!("{root}/a1/b1/c1/rand3.dat")));

    assert!(index_of(&dirs, &format!("{root}/a1/b2")) > -1);
    assert!(index_of(&dirs, &format!("{root}/a1/b2/c1")) > -1);
    assert!(index_of(&dirs, &format!("{root}/a1/b3")) > -1);
    assert!(index_of(&dirs, &format!("{root}/a1/b3/c3")) > -1);

    // Upstream's `QVERIFY(dirs.indexOf(...))` only fails for index 0.
    assert_ne!(index_of(&dirs, &format!("{root}/a2")), 0);
    assert_ne!(index_of(&dirs, &format!("{root}/a2/b3")), 0);
    assert_ne!(index_of(&dirs, &format!("{root}/a2/b3/c3")), 0);

    assert_eq!(dirs.len(), 11, "Directory count wrong.");

    assert!(ok, "findFoldersBelow failed.");
}

#[test]
fn test_stale_watch_descriptor_is_ignored() {
    let mut watcher = FolderWatcherPrivate::default();
    assert!(!watcher.test_watch_contains(i32::MAX));

    let (reader, mut writer) = std::io::pipe().unwrap();

    let name = b"lib\0";
    let mut payload = vec![0u8; INOTIFY_EVENT_SIZE + name.len()];
    payload[0..4].copy_from_slice(&i32::MAX.to_ne_bytes());
    payload[4..8].copy_from_slice(&EventMask::CREATE.bits().to_ne_bytes());
    payload[8..12].copy_from_slice(&0u32.to_ne_bytes());
    payload[12..16].copy_from_slice(&(name.len() as u32).to_ne_bytes());
    payload[INOTIFY_EVENT_SIZE..].copy_from_slice(name);

    writer.write_all(&payload).unwrap();
    watcher
        .slot_received_notification(reader.as_fd(), &mut ())
        .unwrap();

    drop(reader);
    drop(writer);

    assert!(!watcher.test_watch_contains(i32::MAX));
}
