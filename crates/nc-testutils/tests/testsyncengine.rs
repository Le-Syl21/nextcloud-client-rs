// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream `test/testsyncengine.cpp` (nextcloud/desktop v34.0.5).

mod common;

use common::*;
use nc_testutils::{FakeFolder, FileInfo, FileModifier, ItemCompletedSpy};

#[test]
fn test_file_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.remote_modifier().insert("A/a0", 64, b'W');
    fake_folder.sync_once();
    assert!(item_did_complete_successfully(&complete_spy, "A/a0"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_file_upload() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.local_modifier().insert("A/a0", 64, b'W');
    fake_folder.sync_once();
    assert!(item_did_complete_successfully(&complete_spy, "A/a0"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_dir_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.remote_modifier().mkdir("Y");
    fake_folder.remote_modifier().mkdir("Z");
    fake_folder.remote_modifier().insert("Z/d0", 64, b'W');
    fake_folder.sync_once();
    assert!(item_did_complete_successfully(&complete_spy, "Y"));
    assert!(item_did_complete_successfully(&complete_spy, "Z"));
    assert!(item_did_complete_successfully(&complete_spy, "Z/d0"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_dir_upload() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.local_modifier().mkdir("Y");
    fake_folder.local_modifier().mkdir("Z");
    fake_folder.local_modifier().insert("Z/d0", 64, b'W');
    fake_folder.sync_once();
    assert!(item_did_complete_successfully(&complete_spy, "Y"));
    assert!(item_did_complete_successfully(&complete_spy, "Z"));
    assert!(item_did_complete_successfully(&complete_spy, "Z/d0"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

fn local_delete(move_to_trash_enabled: bool) {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let mut sync_options = fake_folder.sync_engine().sync_options().clone();
    sync_options.move_files_to_trash = move_to_trash_enabled;
    fake_folder.sync_engine().set_sync_options(sync_options);
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.remote_modifier().remove("A/a1");
    fake_folder.sync_once();
    assert!(item_did_complete_successfully(&complete_spy, "A/a1"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_local_delete() {
    // _data rows "move to trash" and "delete"
    local_delete(true);
    local_delete(false);
}

#[test]
fn test_remote_delete() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.local_modifier().remove("A/a1");
    fake_folder.sync_once();
    assert!(item_did_complete_successfully(&complete_spy, "A/a1"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}
