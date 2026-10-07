// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testselectivesync.cpp (nextcloud/desktop v34.0.5).

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use common::*;
use nc_journal::journal::SelectiveSyncListType;
use nc_testutils::{FakeFolder, FileInfo, FileModifier};

#[test]
fn test_selective_sync_big_folders() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let mut options = nc_sync::SyncOptions::default();
    // (upstream: SyncEngine::minimumFileAgeForUpload is a static, set once by FakeFolder)
    options.minimum_file_age_for_upload = std::time::Duration::ZERO;
    options.new_big_folder_size_limit = 20000; // 20 K
    fake_folder.sync_engine().set_sync_options(options.clone());

    let size_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let size_requests = size_requests.clone();
        fake_folder.set_server_override(move |req, _| {
            // Record what path we are querying for the size
            if req.method().as_str() == "PROPFIND" && contains_bytes(request_body(req), b"<size ") {
                size_requests
                    .lock()
                    .unwrap()
                    .push(req.uri().path().to_owned());
            }
            None
        });
    }

    let new_big_folder = Rc::new(RefCell::new(Vec::<(String, bool)>::new()));
    {
        let new_big_folder = new_big_folder.clone();
        fake_folder.sync_engine().callbacks().new_big_folder =
            Some(Box::new(move |folder, is_external| {
                new_big_folder
                    .borrow_mut()
                    .push((folder.to_owned(), is_external))
            }));
    }

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.remote_modifier().create_dir("A/newBigDir");
    fake_folder
        .remote_modifier()
        .create_dir("A/newBigDir/subDir");
    fake_folder.remote_modifier().insert(
        "A/newBigDir/subDir/bigFile",
        options.new_big_folder_size_limit + 10,
        b'W',
    );
    fake_folder
        .remote_modifier()
        .insert("A/newBigDir/subDir/smallFile", 10, b'W');

    fake_folder.remote_modifier().create_dir("B/newSmallDir");
    fake_folder
        .remote_modifier()
        .create_dir("B/newSmallDir/subDir");
    fake_folder
        .remote_modifier()
        .insert("B/newSmallDir/subDir/smallFile", 10, b'W');

    // Because the test system don't do that automatically
    {
        let mut remote = fake_folder.remote_modifier();
        remote.find_mut("A/newBigDir").unwrap().extra_dav_properties =
            "<oc:size>20020</oc:size>".to_owned();
        remote
            .find_mut("A/newBigDir/subDir")
            .unwrap()
            .extra_dav_properties = "<oc:size>20020</oc:size>".to_owned();
        remote
            .find_mut("B/newSmallDir")
            .unwrap()
            .extra_dav_properties = "<oc:size>10</oc:size>".to_owned();
        remote
            .find_mut("B/newSmallDir/subDir")
            .unwrap()
            .extra_dav_properties = "<oc:size>10</oc:size>".to_owned();
    }

    assert!(fake_folder.sync_once());

    assert_eq!(new_big_folder.borrow().len(), 1);
    assert_eq!(new_big_folder.borrow()[0].0, "A/newBigDir");
    assert!(!new_big_folder.borrow()[0].1);
    new_big_folder.borrow_mut().clear();

    assert_eq!(size_requests.lock().unwrap().len(), 2); // "A/newBigDir" and "B/newSmallDir";
    assert_eq!(
        size_requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.contains("/subDir"))
            .count(),
        0
    ); // at no point we should request the size of the subdirs
    size_requests.lock().unwrap().clear();

    let old_sync = fake_folder.current_local_state();
    // syncing again should do the same
    fake_folder
        .sync_journal()
        .schedule_path_for_remote_discovery(b"A/newBigDir");
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), old_sync);
    assert_eq!(new_big_folder.borrow().len(), 1); // (since we don't have a real Folder, the files were not added to any list)
    new_big_folder.borrow_mut().clear();
    assert_eq!(size_requests.lock().unwrap().len(), 1); // "A/newBigDir";
    size_requests.lock().unwrap().clear();

    // Simulate that we accept all files by setting a wildcard white list
    fake_folder
        .sync_journal()
        .set_selective_sync_list(SelectiveSyncListType::WhiteList, &["/".to_owned()]);
    fake_folder
        .sync_journal()
        .schedule_path_for_remote_discovery(b"A/newBigDir");
    assert!(fake_folder.sync_once());
    assert_eq!(new_big_folder.borrow().len(), 0);
    assert_eq!(size_requests.lock().unwrap().len(), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_restore_sub_folder_for_data_finger_print() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.local_modifier().mkdir("topFolder");
    fake_folder.local_modifier().mkdir("topFolder/subFolder");
    fake_folder
        .local_modifier()
        .insert("topFolder/subFolder/a", 64, b'W');
    fake_folder.remote_modifier().extra_dav_properties =
        "<oc:data-fingerprint>initial_finger_print</oc:data-fingerprint>".to_owned();

    assert!(fake_folder.sync_once());

    let mkdir_requests_counter = Arc::new(Mutex::new(0));
    {
        let counter = mkdir_requests_counter.clone();
        fake_folder.set_server_override(move |req, _| {
            if req.method().as_str() == "MKCOL" {
                *counter.lock().unwrap() += 1;
            }
            eprintln!("{}", req.method()); // qDebug()
            None
        });
    }

    fake_folder
        .sync_journal()
        .set_selective_sync_list(SelectiveSyncListType::BlackList, &["topFolder".to_owned()]);
    fake_folder.remote_modifier().extra_dav_properties =
        "<oc:data-fingerprint>changed_finger_print</oc:data-fingerprint>".to_owned();

    assert!(fake_folder.sync_once());
    assert_eq!(*mkdir_requests_counter.lock().unwrap(), 0);
}
