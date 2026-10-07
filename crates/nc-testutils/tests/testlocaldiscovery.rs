// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testlocaldiscovery.cpp (nextcloud/desktop v34.0.5).

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use nc_journal::journal::{SelectiveSyncListType, SyncJournalFileRecord};
use nc_sync::discovery::{
    LocalDiscoveryStyle, LocalListingResult, discovery_single_local_directory_job,
};
use nc_sync::{
    Direction, ErrorCategory, Instruction, LocalDiscoveryTracker, LockStatus, Status,
    SyncFileItemPtr,
};
use nc_testutils::{
    FakeFolder, FileInfo, FileModifier, FolderQuota, ItemCompletedSpy, LockChange, LockState,
    from_secs,
};

fn strings(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

/// `connect(itemCompleted, tracker.slotItemCompleted)` and
/// `connect(finished, tracker.slotSyncFinished)`.
fn connect_tracker(fake_folder: &mut FakeFolder) -> Rc<RefCell<LocalDiscoveryTracker>> {
    let tracker = Rc::new(RefCell::new(LocalDiscoveryTracker::new()));
    let t = tracker.clone();
    fake_folder.sync_engine().callbacks().item_completed =
        Some(Box::new(move |item: &SyncFileItemPtr, _: ErrorCategory| {
            t.borrow_mut().slot_item_completed(&item.borrow());
        }));
    let t = tracker.clone();
    fake_folder.sync_engine().callbacks().finished = Some(Box::new(move |success| {
        t.borrow_mut().slot_sync_finished(success);
    }));
    tracker
}

fn tracker_paths(tracker: &Rc<RefCell<LocalDiscoveryTracker>>) -> Vec<String> {
    tracker
        .borrow()
        .local_discovery_paths()
        .iter()
        .cloned()
        .collect()
}

/// `getFileRecord(path, &record)` must succeed; returns the (possibly
/// invalid) record.
fn file_record(fake_folder: &FakeFolder, path: &str) -> SyncJournalFileRecord {
    fake_folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .expect("getFileRecord")
        .unwrap_or_default()
}

#[test]
fn test_file_opened_as_directory_completes_discovery_job() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory");

    let file_name = temporary_directory.path().join("file");
    std::fs::File::create(&file_name).expect("open file");

    // The job runs synchronously here (no thread pool): it must complete with
    // `finished` (an empty listing), not hang nor fail.
    let result = discovery_single_local_directory_job(file_name.to_str().unwrap(), false, |_| {
        panic!("no item expected")
    });
    assert!(
        matches!(&result, LocalListingResult::Finished(v) if v.is_empty()),
        "{result:?}"
    );
}

#[test]
fn test_selective_sync_quota_exceeded_data_loss() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    // folders that fit the quota
    fake_folder.local_modifier().mkdir("big-files");
    fake_folder
        .local_modifier()
        .insert("big-files/bigfile_A.data", 1000, b'W');
    fake_folder
        .local_modifier()
        .insert("big-files/bigfile_B.data", 1000, b'W');
    fake_folder
        .local_modifier()
        .insert("big-files/bigfile_C.data", 1000, b'W');
    fake_folder.local_modifier().mkdir("more-big-files");
    fake_folder
        .local_modifier()
        .insert("more-big-files/bigfile_A.data", 1000, b'W');
    fake_folder
        .local_modifier()
        .insert("more-big-files/bigfile_B.data", 1000, b'W');
    fake_folder
        .local_modifier()
        .insert("more-big-files/bigfile_C.data", 1000, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // folders that won't fit
    fake_folder.local_modifier().mkdir("big-files-wont-fit");
    fake_folder
        .local_modifier()
        .insert("big-files-wont-fit/bigfile_A.data", 800, b'W');
    fake_folder
        .local_modifier()
        .insert("big-files-wont-fit/bigfile_B.data", 800, b'W');
    fake_folder
        .local_modifier()
        .mkdir("more-big-files-wont-fit");
    fake_folder
        .local_modifier()
        .insert("more-big-files-wont-fit/bigfile_A.data", 800, b'W');
    fake_folder
        .local_modifier()
        .insert("more-big-files-wont-fit/bigfile_B.data", 800, b'W');

    let remote_quota = 600;
    fake_folder.set_server_override(move |req, _| {
        if req.method() == http::Method::PUT {
            let total: i64 = req
                .headers()
                .get("OC-Total-Length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if total > remote_quota {
                return Some(nc_testutils::server::error_reply(507, bytes::Bytes::new()).into());
            }
        }
        None
    });

    assert!(!fake_folder.sync_once());

    let check = |fake_folder: &FakeFolder| {
        let local = fake_folder.current_local_state();
        let remote = fake_folder.current_remote_state();
        assert!(local.find("big-files-wont-fit/bigfile_A.data").is_some());
        assert!(local.find("big-files-wont-fit/bigfile_B.data").is_some());
        assert!(
            local
                .find("more-big-files-wont-fit/bigfile_A.data")
                .is_some()
        );
        assert!(
            local
                .find("more-big-files-wont-fit/bigfile_B.data")
                .is_some()
        );
        assert!(remote.find("big-files-wont-fit/bigfile_A.data").is_none());
        assert!(remote.find("big-files-wont-fit/bigfile_B.data").is_none());
        assert!(
            remote
                .find("more-big-files-wont-fit/bigfile_A.data")
                .is_none()
        );
        assert!(
            remote
                .find("more-big-files-wont-fit/bigfile_B.data")
                .is_none()
        );
    };

    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::BlackList,
        &strings(&["big-files-wont-fit/", "more-big-files-wont-fit/"]),
    );
    fake_folder.sync_engine().set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        strings(&["big-files-wont-fit/", "more-big-files-wont-fit/"]),
    );

    assert!(fake_folder.sync_journal().wipe_error_blacklist() != 0);
    assert!(fake_folder.sync_once());
    check(&fake_folder);

    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::BlackList,
        &strings(&[
            "big-files-wont-fit/",
            "more-big-files-wont-fit/",
            "big-files/",
        ]),
    );
    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::WhiteList,
        &strings(&["more-big-files/"]),
    );
    fake_folder.sync_engine().set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        strings(&["big-files/", "more-big-files/"]),
    );

    assert!(fake_folder.sync_once());
    check(&fake_folder);

    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::WhiteList,
        &strings(&["big-files/", "more-big-files/"]),
    );
    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::BlackList,
        &strings(&["big-files-wont-fit/", "more-big-files-wont-fit/"]),
    );
    fake_folder.sync_engine().set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        strings(&["big-files/", "more-big-files/"]),
    );

    assert!(fake_folder.sync_once());
    check(&fake_folder);
}

// Check correct behavior when local discovery is partially drawn from the db
#[test]
fn test_local_discovery_style() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    let tracker = connect_tracker(&mut fake_folder);

    // More subdirectories are useful for testing
    fake_folder.local_modifier().mkdir("A/X");
    fake_folder.local_modifier().mkdir("A/Y");
    fake_folder.local_modifier().insert("A/X/x1", 64, b'W');
    fake_folder.local_modifier().insert("A/Y/y1", 64, b'W');
    tracker.borrow_mut().add_touched_path("A/X");

    tracker.borrow_mut().start_sync_full_discovery();
    assert!(fake_folder.sync_once());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert!(tracker.borrow().local_discovery_paths().is_empty());

    // Test begins
    fake_folder.local_modifier().insert("A/a3", 64, b'W');
    fake_folder.local_modifier().insert("A/X/x2", 64, b'W');
    fake_folder.local_modifier().insert("A/Y/y2", 64, b'W');
    fake_folder.local_modifier().insert("B/b3", 64, b'W');
    fake_folder.remote_modifier().insert("C/c3", 64, b'W');
    fake_folder.remote_modifier().append_byte("C/c1");
    tracker.borrow_mut().add_touched_path("A/X");

    let paths = tracker_paths(&tracker);
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, paths);

    tracker.borrow_mut().start_sync_partial_discovery();
    assert!(fake_folder.sync_once());

    assert!(fake_folder.current_remote_state().find("A/a3").is_some());
    assert!(fake_folder.current_remote_state().find("A/X/x2").is_some());
    assert!(fake_folder.current_remote_state().find("A/Y/y2").is_none());
    assert!(fake_folder.current_remote_state().find("B/b3").is_none());
    assert!(fake_folder.current_local_state().find("C/c3").is_some());
    assert_eq!(
        fake_folder.sync_engine().last_local_discovery_style(),
        LocalDiscoveryStyle::DatabaseAndFilesystem
    );
    assert!(tracker.borrow().local_discovery_paths().is_empty());

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(
        fake_folder.sync_engine().last_local_discovery_style(),
        LocalDiscoveryStyle::FilesystemOnly
    );
    assert!(tracker.borrow().local_discovery_paths().is_empty());
}

#[test]
fn test_local_discovery_decision() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let engine = fake_folder.sync_engine();

    assert!(engine.should_discover_locally(""));
    assert!(engine.should_discover_locally("A"));
    assert!(engine.should_discover_locally("A/X"));

    engine.set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        strings(&[
            "A/X",
            "A/X space",
            "A/X/beta",
            "foo bar space/touch",
            "foo/",
            "zzz",
            "zzzz",
        ]),
    );

    assert!(engine.should_discover_locally(""));
    assert!(engine.should_discover_locally("A"));
    assert!(engine.should_discover_locally("A/X"));
    assert!(!engine.should_discover_locally("B"));
    assert!(!engine.should_discover_locally("A B"));
    assert!(!engine.should_discover_locally("B/X"));
    assert!(engine.should_discover_locally("foo bar space"));
    assert!(engine.should_discover_locally("foo"));
    assert!(!engine.should_discover_locally("foo bar"));
    assert!(!engine.should_discover_locally("foo bar/touch"));
    // These are within "A/X" so they should be discovered
    assert!(engine.should_discover_locally("A/X/alpha"));
    assert!(engine.should_discover_locally("A/X beta"));
    assert!(engine.should_discover_locally("A/X/Y"));
    assert!(engine.should_discover_locally("A/X space"));
    assert!(engine.should_discover_locally("A/X space/alpha"));
    assert!(!engine.should_discover_locally("A/Xylo/foo"));
    assert!(engine.should_discover_locally("zzzz/hello"));
    assert!(!engine.should_discover_locally("zzza/hello"));

    // QEXPECT_FAIL("", "There is a possibility of false positives if the set contains a path "
    //     "which is a prefix, and that prefix is followed by a character less than '/'", Continue);
    // QVERIFY(!engine.shouldDiscoverLocally("A/X o"));
    // The expected failure is asserted as such: an unexpected pass is an
    // XPASS, which fails the upstream test too.
    assert!(
        engine.should_discover_locally("A/X o"),
        "XPASS: the known false positive is gone"
    );

    engine.set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, Vec::new());

    assert!(!engine.should_discover_locally(""));
}

// Check whether item success and item failure adjusts the
// tracker correctly.
#[test]
fn test_tracker_item_completion() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    let tracker = connect_tracker(&mut fake_folder);
    let tracker_contains = |path: &str| tracker.borrow().local_discovery_paths().contains(path);

    tracker.borrow_mut().add_touched_path("A/spurious");

    fake_folder.local_modifier().insert("A/a3", 64, b'W');
    tracker.borrow_mut().add_touched_path("A/a3");

    fake_folder.local_modifier().insert("A/a4", 64, b'W');
    fake_folder.server_error_paths().append("A/a4", 500);
    // We're not adding a4 as touched, it's in the same folder as a3 and will be seen.
    // And due to the error it should be added to the explicit list while a3 gets removed.

    let paths = tracker_paths(&tracker);
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, paths);
    tracker.borrow_mut().start_sync_partial_discovery();
    assert!(!fake_folder.sync_once());

    assert!(fake_folder.current_remote_state().find("A/a3").is_some());
    assert!(fake_folder.current_remote_state().find("A/a4").is_none());
    assert!(!tracker_contains("A/a3"));
    assert!(tracker_contains("A/a4"));
    assert!(tracker_contains("A/spurious")); // not removed since overall sync not successful

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, Vec::new());
    tracker.borrow_mut().start_sync_full_discovery();
    assert!(!fake_folder.sync_once());

    assert!(fake_folder.current_remote_state().find("A/a4").is_none());
    assert!(tracker_contains("A/a4")); // had an error, still here
    assert!(!tracker_contains("A/spurious")); // removed due to full discovery

    fake_folder.server_error_paths().clear();
    assert!(fake_folder.sync_journal().wipe_error_blacklist() != -1);
    tracker.borrow_mut().add_touched_path("A/newspurious"); // will be removed due to successful sync

    let paths = tracker_paths(&tracker);
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, paths);
    tracker.borrow_mut().start_sync_partial_discovery();
    assert!(fake_folder.sync_once());

    assert!(fake_folder.current_remote_state().find("A/a4").is_some());
    assert!(tracker.borrow().local_discovery_paths().is_empty());
}

#[test]
fn test_directory_and_sub_directory() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    fake_folder.local_modifier().mkdir("A/newDir");
    fake_folder.local_modifier().mkdir("A/newDir/subDir");
    fake_folder
        .local_modifier()
        .insert("A/newDir/subDir/file", 10, b'W');

    let expected_state = fake_folder.current_local_state();

    // Only "A" was modified according to the file system tracker
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, strings(&["A"]));

    assert!(fake_folder.sync_once());

    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

// Tests the behavior of invalid filename detection
#[test]
fn test_server_blacklist() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.set_capabilities(serde_json::json!({
        "files": { "blacklisted_files": [".foo", "bar"] }
    }));
    fake_folder.local_modifier().insert("C/.foo", 64, b'W');
    fake_folder.local_modifier().insert("C/bar", 64, b'W');
    fake_folder.local_modifier().insert("C/moo", 64, b'W');
    fake_folder.local_modifier().insert("C/.moo", 64, b'W');

    assert!(fake_folder.sync_once());
    assert!(fake_folder.current_remote_state().find("C/moo").is_some());
    assert!(fake_folder.current_remote_state().find("C/.moo").is_some());
    assert!(fake_folder.current_remote_state().find("C/.foo").is_none());
    assert!(fake_folder.current_remote_state().find("C/bar").is_none());
}

// Tests the behavior of forbidden filename detection
#[test]
fn test_server_forbidden_filenames() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.set_capabilities(serde_json::json!({
        "files": {
            "forbidden_filenames": [".foo", "bar"],
            "forbidden_filename_characters": ["_"],
            "forbidden_filename_basenames": ["base"],
            "forbidden_filename_extensions": ["ext"]
        }
    }));

    for f in [
        "C/.foo",
        "C/bar",
        "C/moo",
        "C/.moo",
        "C/potatopotato.txt",
        "C/potato_potato.txt",
        "C/basefilename.txt",
        "C/base.txt",
        "C/filename.txt",
        "C/filename.ext",
    ] {
        fake_folder.local_modifier().insert(f, 64, b'W');
    }

    assert!(fake_folder.sync_once());
    let remote = fake_folder.current_remote_state();
    assert!(remote.find("C/moo").is_some());
    assert!(remote.find("C/.moo").is_some());
    assert!(remote.find("C/.foo").is_none());
    assert!(remote.find("C/bar").is_none());
    assert!(remote.find("C/potatopotato.txt").is_some());
    assert!(remote.find("C/potato_potato.txt").is_none());
    assert!(remote.find("C/basefilename.txt").is_some());
    assert!(remote.find("C/base.txt").is_none());
    assert!(remote.find("C/filename.txt").is_some());
    assert!(remote.find("C/filename.ext").is_none());
}

#[test]
fn test_redownload_deleted_live_photo_mov() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let live_photo_img = "IMG_0001.heic";
    let live_photo_mov = "IMG_0001.mov";
    fake_folder
        .local_modifier()
        .insert(live_photo_img, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(live_photo_mov, 64, b'W');

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item(live_photo_img).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(live_photo_mov).status,
        Status::Success
    );

    fake_folder
        .remote_modifier()
        .set_is_live_photo(live_photo_img, true);
    fake_folder
        .remote_modifier()
        .set_is_live_photo(live_photo_mov, true);
    assert!(fake_folder.sync_once());

    let img_record = file_record(&fake_folder, live_photo_img);
    assert!(img_record.is_live_photo);

    let mov_record = file_record(&fake_folder, live_photo_mov);
    assert!(mov_record.is_live_photo);

    complete_spy.clear();
    fake_folder.local_modifier().remove(live_photo_mov);
    assert!(fake_folder.sync_once());
    let item = complete_spy.find_item(live_photo_mov);
    assert_eq!(item.status, Status::Success);
    assert_eq!(item.instruction, Instruction::Sync);
    assert_eq!(item.direction, Direction::Down);
}

#[test]
fn test_create_file_with_trailing_spaces_local_and_remote_trimmed_do_not_exist_rename_and_upload_file()
 {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.enable_enforce_windows_file_name_compatibility();

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let file_with_spaces1 = " foo";
    let file_with_spaces2 = " bar ";
    let file_with_spaces3 = "bla ";
    let file_with_spaces4 = "A/ foo";
    let file_with_spaces5 = "A/ bar ";
    let file_with_spaces6 = "A/bla ";
    let extra_file_name_with_spaces = " with spaces ";

    fake_folder
        .local_modifier()
        .insert(file_with_spaces1, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces2, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces3, 64, b'W');
    fake_folder.local_modifier().mkdir("A");
    fake_folder
        .local_modifier()
        .insert(file_with_spaces4, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces5, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces6, 64, b'W');
    fake_folder
        .local_modifier()
        .mkdir(extra_file_name_with_spaces);

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    complete_spy.clear();

    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item(file_with_spaces1).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces2).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces3).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces4).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces5).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces6).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(extra_file_name_with_spaces).status,
        Status::FileNameInvalid
    );

    let local_path = fake_folder.local_path();
    for f in [
        file_with_spaces1,
        file_with_spaces2,
        file_with_spaces3,
        file_with_spaces4,
        file_with_spaces5,
        file_with_spaces6,
        extra_file_name_with_spaces,
    ] {
        fake_folder
            .sync_engine()
            .add_accepted_invalid_file_name(&format!("{local_path}{f}"));
    }

    complete_spy.clear();

    fake_folder.sync_engine().set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        strings(&["foo", "bar", "bla", "A/foo", "A/bar", "A/bla"]),
    );
    assert!(fake_folder.sync_once());

    // #if !defined Q_OS_WINDOWS
    assert_eq!(
        complete_spy.find_item(file_with_spaces1).status,
        Status::NoStatus
    );
    assert_eq!(complete_spy.find_item(" bar").status, Status::Success);
    assert_eq!(complete_spy.find_item("bla").status, Status::Success);
    assert_eq!(
        complete_spy.find_item(file_with_spaces4).status,
        Status::NoStatus
    );
    assert_eq!(complete_spy.find_item("A/ bar").status, Status::Success);
    assert_eq!(complete_spy.find_item("A/bla").status, Status::Success);
    assert_eq!(
        complete_spy.find_item(" with spaces").status,
        Status::Success
    );
}

#[test]
fn test_create_file_with_trailing_spaces_remote_dont_get_renamed_automatically() {
    // On Windows we can't create files/folders with leading/trailing spaces locally. So, we have to fail those items. On other OSs - we just sync them down normally.
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let file_with_spaces4 = "A/ foo";
    let file_with_spaces5 = "A/ bar ";
    let file_with_spaces6 = "A/bla ";

    fake_folder.remote_modifier().mkdir("A");
    fake_folder
        .remote_modifier()
        .insert(file_with_spaces4, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(file_with_spaces5, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(file_with_spaces6, 64, b'W');

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    complete_spy.clear();

    assert!(fake_folder.sync_once());

    // #else (not Q_OS_WINDOWS)
    assert_eq!(
        complete_spy.find_item(file_with_spaces4).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces5).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces6).status,
        Status::Success
    );
}

#[test]
fn test_create_local_paths_with_leading_and_trailing_spaces_sync_on_supporting_os() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.enable_enforce_windows_file_name_compatibility();
    fake_folder.local_modifier().mkdir("A");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let file_with_spaces1 = "A/ space";
    let file_with_spaces2 = "A/ space ";
    let file_with_spaces3 = "A/space ";
    let folder_with_spaces1 = "A ";
    let folder_with_spaces2 = " B ";

    fake_folder
        .local_modifier()
        .insert(file_with_spaces1, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces2, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces3, 64, b'W');
    fake_folder.local_modifier().mkdir(folder_with_spaces1);
    fake_folder.local_modifier().mkdir(folder_with_spaces2);

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    complete_spy.clear();

    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item(file_with_spaces1).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces2).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces3).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(folder_with_spaces1).status,
        Status::FileNameInvalid
    );
    assert_eq!(
        complete_spy.find_item(folder_with_spaces2).status,
        Status::FileNameInvalid
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find(file_with_spaces1)
            .is_some()
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find(file_with_spaces2)
            .is_none()
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find(file_with_spaces3)
            .is_none()
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find(folder_with_spaces1)
            .is_none()
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find(folder_with_spaces2)
            .is_none()
    );
}

#[test]
fn test_create_file_with_trailing_spaces_remote_get_renamed_manually() {
    // On Windows we can't create files/folders with leading/trailing spaces locally. So, we have to fail those items. On other OSs - we just sync them down normally.
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let file_with_spaces4 = "A/ foo";
    let file_with_spaces5 = "A/ bar ";
    let file_with_spaces6 = "A/bla ";

    let file_without_spaces4 = "A/foo";
    let file_without_spaces5 = "A/bar";
    let file_without_spaces6 = "A/bla";

    fake_folder.remote_modifier().mkdir("A");
    fake_folder
        .remote_modifier()
        .insert(file_with_spaces4, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(file_with_spaces5, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(file_with_spaces6, 64, b'W');

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    complete_spy.clear();

    assert!(fake_folder.sync_once());

    // #else (not Q_OS_WINDOWS)
    assert_eq!(
        complete_spy.find_item(file_with_spaces4).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces5).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_with_spaces6).status,
        Status::Success
    );

    fake_folder
        .remote_modifier()
        .rename(file_with_spaces4, file_without_spaces4);
    fake_folder
        .remote_modifier()
        .rename(file_with_spaces5, file_without_spaces5);
    fake_folder
        .remote_modifier()
        .rename(file_with_spaces6, file_without_spaces6);

    complete_spy.clear();

    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item(file_without_spaces4).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_without_spaces5).status,
        Status::Success
    );
    assert_eq!(
        complete_spy.find_item(file_without_spaces6).status,
        Status::Success
    );
}

#[test]
fn test_create_file_with_trailing_spaces_local_trimmed_also_created_dont_rename_automatically_and_dont_upload_file()
 {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.enable_enforce_windows_file_name_compatibility();
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let file_with_spaces = "foo ";
    let file_trimmed = "foo";

    fake_folder.local_modifier().insert(file_trimmed, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces, 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(
        fake_folder
            .current_remote_state()
            .find(file_trimmed)
            .is_some()
    );

    // no file with spaces on Windows
    assert!(
        fake_folder
            .current_remote_state()
            .find(file_with_spaces)
            .is_none()
    );

    assert!(
        fake_folder
            .current_local_state()
            .find(file_with_spaces)
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find(file_trimmed)
            .is_some()
    );
}

#[test]
fn test_create_file_with_trailing_spaces_local_trimmed_also_created_dont_rename_automatically_and_upload_both_files()
 {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let file_with_spaces = " foo";
    let file_trimmed = "foo";

    fake_folder.local_modifier().insert(file_trimmed, 64, b'W');
    fake_folder
        .local_modifier()
        .insert(file_with_spaces, 64, b'W');

    let local_path = fake_folder.local_path();
    fake_folder
        .sync_engine()
        .add_accepted_invalid_file_name(&format!("{local_path}{file_with_spaces}"));

    assert!(fake_folder.sync_once());

    assert!(
        fake_folder
            .current_remote_state()
            .find(file_trimmed)
            .is_some()
    );
    assert!(
        fake_folder
            .current_remote_state()
            .find(file_with_spaces)
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find(file_with_spaces)
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find(file_trimmed)
            .is_some()
    );
}

#[test]
fn test_create_file_with_trailing_spaces_local_and_remote_trimmed_exists_rename_file() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let file_with_spaces1 = " foo";
    let file_with_spaces2 = " bar ";
    let file_with_spaces3 = "bla ";

    for f in [file_with_spaces1, file_with_spaces2, file_with_spaces3] {
        fake_folder.local_modifier().insert(f, 64, b'W');
    }
    for f in [file_with_spaces1, file_with_spaces2, file_with_spaces3] {
        fake_folder.remote_modifier().insert(f, 64, b'W');
    }

    assert!(fake_folder.sync_once());

    assert!(fake_folder.sync_once());

    assert!(fake_folder.sync_once());

    // #if !defined Q_OS_WINDOWS
    let expected_state = fake_folder.current_local_state();
    eprintln!("{expected_state:?}");
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

const INVALID_MODTIME1: i64 = 0;
const INVALID_MODTIME2: i64 = -3600;

const MTIME_FILES: [&str; 6] = [
    "foo",
    "bar",
    "bla",
    "subfolder/foo",
    "subfolder/bar",
    "subfolder/bla",
];

fn insert_mtime_files(fake_folder: &FakeFolder) {
    let mut remote = fake_folder.remote_modifier();
    remote.insert("foo", 64, b'W');
    remote.insert("bar", 64, b'W');
    remote.insert("bla", 64, b'W');
    remote.mkdir("subfolder");
    remote.insert("subfolder/foo", 64, b'W');
    remote.insert("subfolder/bar", 64, b'W');
    remote.insert("subfolder/bla", 64, b'W');
}

#[test]
fn test_block_invalid_mtime_sync_remote() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    insert_mtime_files(&fake_folder);

    assert!(fake_folder.sync_once());

    for f in MTIME_FILES {
        fake_folder
            .remote_modifier()
            .set_mod_time(f, from_secs(INVALID_MODTIME1));
    }

    assert!(!fake_folder.sync_once());

    assert!(!fake_folder.sync_once());

    for f in MTIME_FILES {
        fake_folder
            .remote_modifier()
            .set_mod_time(f, from_secs(INVALID_MODTIME2));
    }

    assert!(!fake_folder.sync_once());

    assert!(!fake_folder.sync_once());
}

#[test]
fn test_block_invalid_mtime_sync_local() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    let n_get = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let n_get = n_get.clone();
        fake_folder.set_server_override(move |req, _| {
            if req.method() == http::Method::GET {
                n_get.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            None
        });
    }
    let n_get_value = || n_get.load(std::sync::atomic::Ordering::SeqCst);

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    insert_mtime_files(&fake_folder);

    assert!(fake_folder.sync_once());
    n_get.store(0, std::sync::atomic::Ordering::SeqCst);

    for f in MTIME_FILES {
        fake_folder
            .local_modifier()
            .set_mod_time(f, from_secs(INVALID_MODTIME1));
    }

    assert!(fake_folder.sync_once());
    assert_eq!(n_get_value(), 0);

    assert!(fake_folder.sync_once());
    assert_eq!(n_get_value(), 0);

    for f in MTIME_FILES {
        fake_folder
            .local_modifier()
            .set_mod_time(f, from_secs(INVALID_MODTIME2));
    }

    assert!(fake_folder.sync_once());
    assert_eq!(n_get_value(), 0);

    assert!(fake_folder.sync_once());
    assert_eq!(n_get_value(), 0);
}

const FUTURE_MTIME: i64 = 0xFFFF_FFFF;
const CURRENT_MTIME: i64 = 1646057277;

const FUTURE_FILES: [&str; 6] = [
    "foo",
    "bar",
    "subfolder/foo",
    "subfolder/bar",
    "aaa/subfolder/foo",
    "aaa/subfolder/bar",
];

fn insert_future_files(fake_folder: &FakeFolder) {
    let mut remote = fake_folder.remote_modifier();
    remote.insert("foo", 64, b'W');
    remote.insert("bar", 64, b'W');
    remote.mkdir("subfolder");
    remote.insert("subfolder/foo", 64, b'W');
    remote.insert("subfolder/bar", 64, b'W');
    remote.mkdir("aaa");
    remote.mkdir("aaa/subfolder");
    remote.insert("aaa/subfolder/foo", 64, b'W');
    remote.insert("aaa/subfolder/bar", 64, b'W');
}

#[test]
fn test_do_not_sync_invalid_future_mtime() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    insert_future_files(&fake_folder);

    assert!(fake_folder.sync_once());

    for f in FUTURE_FILES {
        fake_folder
            .remote_modifier()
            .set_mod_time(f, from_secs(FUTURE_MTIME));
    }
    for f in FUTURE_FILES {
        fake_folder
            .local_modifier()
            .set_mod_time(f, from_secs(CURRENT_MTIME));
    }

    assert!(!fake_folder.sync_once());
}

#[test]
fn test_invalid_future_mtime_recovery() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    insert_future_files(&fake_folder);

    assert!(fake_folder.sync_once());

    for f in FUTURE_FILES {
        fake_folder
            .remote_modifier()
            .set_mod_time_keep_etag(f, from_secs(CURRENT_MTIME));
    }
    for f in FUTURE_FILES {
        fake_folder
            .local_modifier()
            .set_mod_time(f, from_secs(FUTURE_MTIME));
    }

    assert!(fake_folder.sync_once());

    assert!(fake_folder.sync_once());

    let expected_state = fake_folder.current_local_state();
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

#[test]
fn test_discover_lock_changes() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.set_capabilities(serde_json::json!({
        "activity": { "apiv2": ["filters", "filters-api", "previews", "rich-strings"] },
        "bruteforce": { "delay": 0 },
        "core": { "pollinterval": 60, "webdav-root": "remote.php/webdav" },
        "dav": { "chunking": "1.0" },
        "files": {
            "bigfilechunking": true,
            "blacklisted_files": [".htaccess"],
            "comments": true,
            "directEditing": {
                "etag": "c748e8fc588b54fc5af38c4481a19d20",
                "url": "https://nextcloud.local/ocs/v2.php/apps/files/api/v1/directEditing"
            },
            "locking": "1.0"
        }
    }));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let foo_file_root_folder = "foo";
    let bar_file_root_folder = "bar";
    let foo_file_sub_folder = "subfolder/foo";
    let bar_file_sub_folder = "subfolder/bar";
    let foo_file_aaa_sub_folder = "aaa/subfolder/foo";
    let bar_file_aaa_sub_folder = "aaa/subfolder/bar";

    {
        let mut remote = fake_folder.remote_modifier();
        remote.insert(foo_file_root_folder, 64, b'W');
        remote.insert(bar_file_root_folder, 64, b'W');
        remote.modify_lock_state(
            "bar",
            LockChange {
                lock_state: LockState::FileLocked,
                lock_type: 0,
                lock_owner: "user1".to_owned(),
                lock_owner_id: String::new(),
                lock_editor_id: "user1".to_owned(),
                lock_time: 1648046707,
                lock_timeout: 0,
            },
        );

        remote.mkdir("subfolder");
        remote.insert(foo_file_sub_folder, 64, b'W');
        remote.insert(bar_file_sub_folder, 64, b'W');
        remote.mkdir("aaa");
        remote.mkdir("aaa/subfolder");
        remote.insert(foo_file_aaa_sub_folder, 64, b'W');
        remote.insert(bar_file_aaa_sub_folder, 64, b'W');
    }

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    complete_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(complete_spy.find_item("bar").locked, LockStatus::LockedItem);
    let file_record_before = file_record(&fake_folder, "bar");
    assert!(file_record_before.lockstate.locked);

    fake_folder.remote_modifier().modify_lock_state(
        "bar",
        LockChange {
            lock_state: LockState::FileUnlocked,
            ..Default::default()
        },
    );

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, Vec::new());

    complete_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(
        complete_spy.find_item("bar").locked,
        LockStatus::UnlockedItem
    );
    let file_record_after = file_record(&fake_folder, "bar");
    assert!(!file_record_after.lockstate.locked);
}

#[test]
fn test_discovery_uses_correct_quota_source() {
    //setup sync folder
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    // create folder
    let folder_a = "A";
    fake_folder.local_modifier().mkdir(folder_a);
    fake_folder.remote_modifier().mkdir(folder_a);
    fake_folder.remote_modifier().set_folder_quota(
        folder_a,
        FolderQuota::new(0, 500),
        nc_testutils::EtagsAction::Keep,
    );

    // sync folderA
    let sync_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(folder_a).status, Status::NoStatus);

    // check db quota for folderA - bytesAvailable is 500
    let record_folder_a = file_record(&fake_folder, folder_a);
    assert_eq!(record_folder_a.folder_quota.bytes_available, 500);

    // add fileNameA to folderA - size < quota in db
    let file_name_a = "A/A.data";
    fake_folder.local_modifier().insert(file_name_a, 200, b'W');

    // set different quota for folderA - remote change does not change etag yet
    fake_folder.remote_modifier().set_folder_quota(
        folder_a,
        FolderQuota::new(0, 0),
        nc_testutils::EtagsAction::Keep,
    );

    // sync filenameA - size == quota => success
    sync_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(file_name_a).status, Status::Success);

    // add smallFile to folderA - size < quota in db
    let small_file = "A/smallFile.data";
    fake_folder.local_modifier().insert(small_file, 100, b'W');

    // sync smallFile - size < quota in db => success => update quota in db
    sync_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(small_file).status, Status::Success);

    // create remoteFileA - size > bytes available
    let remote_file_a = "A/remoteA.data";
    fake_folder
        .remote_modifier()
        .insert(remote_file_a, 200, b'W');

    // sync remoteFile - it is a download
    sync_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(remote_file_a).status, Status::Success);

    // check db quota for folderA - bytesAvailable have changed to 0 due to new PROPFIND
    let record_folder_a = file_record(&fake_folder, folder_a);
    assert_eq!(record_folder_a.folder_quota.bytes_available, 0);

    // create local fileNameB - size < quota in db
    let file_name_b = "A/B.data";
    fake_folder.local_modifier().insert(file_name_b, 0, b'W');

    // set different quota for folderA - remote change does not change etag yet
    fake_folder.remote_modifier().set_folder_quota(
        folder_a,
        FolderQuota::new(500, 600),
        nc_testutils::EtagsAction::Keep,
    );

    // sync fileNameB - size < quota in db => success
    sync_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(file_name_b).status, Status::Success);

    // create remoteFileB - it is a download
    let remote_file_b = "A/remoteB.data";
    fake_folder
        .remote_modifier()
        .insert(remote_file_a, 100, b'W');

    // create local fileNameC - size < quota in db
    let file_name_c = "A/C.data";
    fake_folder.local_modifier().insert(file_name_c, 0, b'W');

    // sync filenameC - size < quota in db => success
    sync_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(file_name_c).status, Status::Success);

    // check db quota for folderA - bytesAvailable have changed to 600 due to new PROPFIND
    let record_folder_a = file_record(&fake_folder, folder_a);
    assert_eq!(record_folder_a.folder_quota.bytes_available, 600);

    assert_eq!(sync_spy.find_item(remote_file_b).status, Status::NoStatus);

    // create local fileNameD - size > quota in db
    let file_name_d = "A/D.data";
    fake_folder.local_modifier().insert(file_name_d, 700, b'W');

    // sync fileNameD - size > quota in db => error
    sync_spy.clear();
    assert!(!fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(file_name_d).status, Status::NormalError);

    // create local fileNameE - size < quota in db
    let file_name_e = "A/E.data";
    fake_folder.local_modifier().insert(file_name_e, 400, b'W');

    // sync fileNameE - size < quota in db => success
    sync_spy.clear();
    assert!(!fake_folder.sync_once());
    assert_eq!(sync_spy.find_item(file_name_e).status, Status::Success);
}

#[test]
fn test_missing_inode_and_fix_them() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("A");
        remote.mkdir("A/B");
        remote.insert("textFile.txt", 64, b'W');
        remote.insert("document.md", 64, b'W');
        remote.insert("A/B/sub-folder.md", 64, b'W');
    }

    assert!(fake_folder.sync_once());

    let paths = [
        "document.md",
        "textFile.txt",
        "A",
        "A/B",
        "A/B/sub-folder.md",
    ];
    for path in paths {
        let mut record = file_record(&fake_folder, path);
        assert!(record.inode > 0, "{path}");
        record.inode = 0;
        assert!(fake_folder.sync_journal().set_file_record(&record).is_ok());
    }

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, Vec::new());
    assert!(fake_folder.sync_once());

    for path in paths {
        let record = file_record(&fake_folder, path);
        assert!(record.inode > 0, "{path}");
    }
}
