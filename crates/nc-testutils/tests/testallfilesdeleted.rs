// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testallfilesdeleted.cpp (nextcloud/desktop v34.0.5).
//
// Upstream calls `ConfigFile().setPromptDeleteFiles(true)` in the first test;
// the setting is persisted in the test config file, so every following test
// of the binary runs with it enabled (and testResetServer relies on it). Each
// Rust test sets `prompt_delete_files = true` on its own engine instead.

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use common::*;
use nc_journal::journal::SelectiveSyncListType;
use nc_sync::Direction;
use nc_testutils::{
    FakeFolder, FileInfo, FileModifier, find_conflict, generate_etag, generate_file_id,
};

fn change_all_file_id(info: &mut FileInfo) {
    info.file_id = generate_file_id();
    if !info.is_dir {
        return;
    }
    info.etag = generate_etag();
    for child in info.children.values_mut() {
        change_all_file_id(child);
    }
}

fn new_folder(template: FileInfo) -> FakeFolder {
    let mut fake_folder = FakeFolder::new(template);
    // ConfigFile().setPromptDeleteFiles(true)
    fake_folder.sync_engine().prompt_delete_files = true;
    fake_folder
}

fn remove_all_top_level(fake_folder: &mut FakeFolder, delete_on_remote: bool) {
    let children_keys: Vec<String> = fake_folder
        .current_remote_state()
        .children
        .keys()
        .cloned()
        .collect();
    for key in &children_keys {
        if delete_on_remote {
            fake_folder.remote_modifier().remove(key);
        } else {
            fake_folder.local_modifier().remove(key);
        }
    }
}

/*
 * In this test, all files are deleted in the client, or the server, and we simulate
 * that the users press "keep"
 */
#[test]
fn test_all_files_deleted_keep() {
    // _data rows: "local" (false), "remote" (true)
    for delete_on_remote in [false, true] {
        let mut fake_folder = new_folder(FileInfo::A12_B12_C12_S12());

        //Just set a blacklist so we can check it is still there. This directory does not exists but
        // that does not matter for our purposes.
        let selective_sync_black_list = vec!["Q/".to_owned()];
        fake_folder
            .sync_journal()
            .set_selective_sync_list(SelectiveSyncListType::BlackList, &selective_sync_black_list);

        let initial_state = fake_folder.current_local_state();
        let about_to_remove_all_files_called = Rc::new(Cell::new(0));
        {
            let called = about_to_remove_all_files_called.clone();
            let journal = fake_folder.sync_engine().journal().clone();
            fake_folder
                .sync_engine()
                .callbacks()
                .about_to_remove_all_files = Some(Box::new(move |dir| {
                assert_eq!(called.get(), 0);
                called.set(called.get() + 1);
                assert_eq!(
                    dir,
                    if delete_on_remote {
                        Direction::Down
                    } else {
                        Direction::Up
                    }
                );
                // callback(true)
                journal.clear_file_table(); // That's what Folder is doing
                true
            }));
        }

        remove_all_top_level(&mut fake_folder, delete_on_remote);

        assert!(!fake_folder.sync_once()); // Should fail because we cancel the sync
        assert_eq!(about_to_remove_all_files_called.get(), 1);

        // Next sync should recover all files
        assert!(fake_folder.sync_once());
        assert_eq!(fake_folder.current_local_state(), initial_state);
        assert_eq!(fake_folder.current_remote_state(), initial_state);

        // The selective sync blacklist should be not have been deleted.
        assert_eq!(
            fake_folder
                .sync_journal()
                .get_selective_sync_list(SelectiveSyncListType::BlackList)
                .unwrap(),
            selective_sync_black_list
        );
    }
}

/*
 * This test is like the previous one but we simulate that the user presses "delete"
 */
#[test]
fn test_all_files_deleted_delete() {
    // _data rows: "local" (false), "remote" (true)
    for delete_on_remote in [false, true] {
        let mut fake_folder = new_folder(FileInfo::A12_B12_C12_S12());

        let about_to_remove_all_files_called = Rc::new(Cell::new(0));
        {
            let called = about_to_remove_all_files_called.clone();
            fake_folder
                .sync_engine()
                .callbacks()
                .about_to_remove_all_files = Some(Box::new(move |dir| {
                assert_eq!(called.get(), 0);
                called.set(called.get() + 1);
                assert_eq!(
                    dir,
                    if delete_on_remote {
                        Direction::Down
                    } else {
                        Direction::Up
                    }
                );
                // callback(false)
                false
            }));
        }

        remove_all_top_level(&mut fake_folder, delete_on_remote);

        assert!(fake_folder.sync_once()); // Should succeed, and all files must then be deleted

        assert_eq!(
            fake_folder.current_local_state(),
            fake_folder.current_remote_state()
        );
        assert_eq!(fake_folder.current_local_state().children.len(), 0);

        // Try another sync to be sure.

        assert!(fake_folder.sync_once()); // Should succeed (doing nothing)
        assert_eq!(about_to_remove_all_files_called.get(), 1); // should not have been called.

        assert_eq!(
            fake_folder.current_local_state(),
            fake_folder.current_remote_state()
        );
        assert_eq!(fake_folder.current_local_state().children.len(), 0);
    }
}

#[test]
fn test_not_delete_meta_data_change() {
    /*
     * This test make sure that we don't popup a file deleted message if all the metadata have
     * been updated (for example when the server is upgraded or something)
     */

    let mut fake_folder = new_folder(FileInfo::A12_B12_C12_S12());
    // We never remove all files.
    fake_folder
        .sync_engine()
        .callbacks()
        .about_to_remove_all_files = Some(Box::new(|_| {
        panic!("aboutToRemoveAllFiles must not be emitted")
    }));
    assert!(fake_folder.sync_once());

    let children_keys: Vec<String> = fake_folder
        .current_remote_state()
        .children
        .keys()
        .cloned()
        .collect();
    for s in &children_keys {
        fake_folder
            .sync_journal()
            .avoid_renames_on_next_sync(s.as_bytes()); // clears all the fileid and inodes.
    }
    fake_folder.local_modifier().remove("A/a1");
    let mut expected_state = fake_folder.current_local_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);

    fake_folder.remote_modifier().remove("B/b1");
    change_all_file_id(&mut fake_folder.remote_modifier());
    expected_state = fake_folder.current_remote_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

#[test]
fn test_reset_server() {
    let mut fake_folder = new_folder(FileInfo::A12_B12_C12_S12());

    let about_to_remove_all_files_called = Rc::new(Cell::new(0));
    {
        let called = about_to_remove_all_files_called.clone();
        fake_folder
            .sync_engine()
            .callbacks()
            .about_to_remove_all_files = Some(Box::new(move |dir| {
            assert_eq!(called.get(), 0);
            called.set(called.get() + 1);
            assert_eq!(dir, Direction::Down);
            // callback(false)
            false
        }));
    }

    // Some small changes
    fake_folder.local_modifier().mkdir("Q");
    fake_folder.local_modifier().insert("Q/q1", 64, b'W');
    fake_folder.local_modifier().append_byte("B/b1");
    assert!(fake_folder.sync_once());
    assert_eq!(about_to_remove_all_files_called.get(), 0);

    // Do some change locally
    fake_folder.local_modifier().append_byte("A/a1");

    // reset the server.
    *fake_folder.remote_modifier() = FileInfo::A12_B12_C12_S12();

    // Now, aboutToRemoveAllFiles with down as a direction
    assert!(fake_folder.sync_once());
    assert_eq!(about_to_remove_all_files_called.get(), 1);
}

#[test]
fn test_data_finget_print() {
    // _data rows: "initial finger print" (true), "no initial finger print" (false)
    for has_initial_finger_print in [true, false] {
        data_finger_print(has_initial_finger_print);
    }
}

fn days_from_now(days: i64) -> std::time::SystemTime {
    nc_testutils::from_secs(nc_sync::utility::current_secs_since_epoch() + days * 24 * 3600)
}

fn data_finger_print(has_initial_finger_print: bool) {
    let mut fake_folder = new_folder(FileInfo::A12_B12_C12_S12());
    fake_folder.remote_modifier().set_contents("C/c1", b'N');
    fake_folder
        .remote_modifier()
        .set_mod_time("C/c1", days_from_now(-2));
    fake_folder.remote_modifier().remove("C/c2");
    if has_initial_finger_print {
        fake_folder.remote_modifier().extra_dav_properties =
            "<oc:data-fingerprint>initial_finger_print</oc:data-fingerprint>".to_owned();
    } else {
        //Server support finger print, but none is set.
        fake_folder.remote_modifier().extra_dav_properties =
            "<oc:data-fingerprint></oc:data-fingerprint>".to_owned();
    }

    let fingerprint_requests = Arc::new(Mutex::new(0i32));
    {
        let fingerprint_requests = fingerprint_requests.clone();
        fake_folder.set_server_override(move |request, _| {
            if request.method().as_str() == "PROPFIND" {
                let data = request_body(request);
                if contains_bytes(data, b"data-fingerprint") {
                    let mut n = fingerprint_requests.lock().unwrap();
                    if request.uri().path().ends_with("dav/files/admin/") {
                        *n += 1;
                    } else {
                        *n = -10000; // fingerprint queried on incorrect path
                    }
                }
            }
            None
        });
    }

    assert!(fake_folder.sync_once());
    assert_eq!(*fingerprint_requests.lock().unwrap(), 1);
    // First sync, we did not change the finger print, so the file should be downloaded as normal
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(
        fake_folder
            .current_remote_state()
            .find("C/c1")
            .unwrap()
            .content_char,
        b'N'
    );
    assert!(fake_folder.current_remote_state().find("C/c2").is_none());

    /* Simulate a backup restoration */

    // A/a1 is an old file
    fake_folder.remote_modifier().set_contents("A/a1", b'O');
    fake_folder
        .remote_modifier()
        .set_mod_time("A/a1", days_from_now(-2));
    // B/b1 did not exist at the time of the backup
    fake_folder.remote_modifier().remove("B/b1");
    // B/b2 was uploaded by another user in the mean time.
    fake_folder.remote_modifier().set_contents("B/b2", b'N');
    fake_folder
        .remote_modifier()
        .set_mod_time("B/b2", days_from_now(2));

    // C/c3 was removed since we made the backup
    fake_folder
        .remote_modifier()
        .insert("C/c3_removed", 64, b'W');
    // C/c4 was moved to A/a2 since we made the backup
    fake_folder
        .remote_modifier()
        .rename("A/a2", "C/old_a2_location");

    // The admin sets the data-fingerprint property
    fake_folder.remote_modifier().extra_dav_properties =
        "<oc:data-fingerprint>new_finger_print</oc:data-fingerprint>".to_owned();

    assert!(fake_folder.sync_once());
    assert_eq!(*fingerprint_requests.lock().unwrap(), 2);
    let current_state = fake_folder.current_local_state();
    // Although the local file is kept as a conflict, the server file is downloaded
    assert_eq!(current_state.find("A/a1").unwrap().content_char, b'O');
    let conflict = find_conflict(&current_state, "A/a1");
    assert!(conflict.is_some());
    let conflict = conflict.unwrap();
    assert_eq!(conflict.content_char, b'W');
    fake_folder.local_modifier().remove(&conflict.path());
    // b1 was restored (re-uploaded)
    assert!(current_state.find("B/b1").is_some());

    // b2 has the new content (was not restored), since its mode time goes forward in time
    assert_eq!(current_state.find("B/b2").unwrap().content_char, b'N');
    let conflict = find_conflict(&current_state, "B/b2");
    assert!(conflict.is_some()); // Just to be sure, we kept the old file in a conflict
    let conflict = conflict.unwrap();
    assert_eq!(conflict.content_char, b'W');
    fake_folder.local_modifier().remove(&conflict.path());

    // We actually do not remove files that technically should have been removed (we don't want data-loss)
    assert!(current_state.find("C/c3_removed").is_some());
    assert!(current_state.find("C/old_a2_location").is_some());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_single_file_renamed() {
    let mut fake_folder = new_folder(FileInfo::default());

    let about_to_remove_all_files_called = Rc::new(Cell::new(0));
    {
        let called = about_to_remove_all_files_called.clone();
        fake_folder
            .sync_engine()
            .callbacks()
            .about_to_remove_all_files = Some(Box::new(move |_| {
            called.set(called.get() + 1);
            panic!("should not be called");
        }));
    }

    // add a single file
    fake_folder.local_modifier().insert("hello.txt", 64, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(about_to_remove_all_files_called.get(), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // rename it
    fake_folder
        .local_modifier()
        .rename("hello.txt", "goodbye.txt");

    assert!(fake_folder.sync_once());
    assert_eq!(about_to_remove_all_files_called.get(), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_selective_sync_no_popup() {
    // Unselecting all folder should not cause the popup to be shown
    let mut fake_folder = new_folder(FileInfo::A12_B12_C12_S12());

    let about_to_remove_all_files_called = Rc::new(Cell::new(0));
    {
        let called = about_to_remove_all_files_called.clone();
        fake_folder
            .sync_engine()
            .callbacks()
            .about_to_remove_all_files = Some(Box::new(move |_| {
            called.set(called.get() + 1);
            panic!("should not be called");
        }));
    }

    assert!(fake_folder.sync_once());
    assert_eq!(about_to_remove_all_files_called.get(), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::BlackList,
        &[
            "A/".to_owned(),
            "B/".to_owned(),
            "C/".to_owned(),
            "S/".to_owned(),
        ],
    );

    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), FileInfo::default()); // all files should be one locally
    assert_eq!(
        fake_folder.current_remote_state(),
        FileInfo::A12_B12_C12_S12()
    ); // Server not changed
    assert_eq!(about_to_remove_all_files_called.get(), 0); // But we did not show the popup
}
