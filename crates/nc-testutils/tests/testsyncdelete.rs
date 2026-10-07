// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testsyncdelete.cpp (nextcloud/desktop v34.0.5).

use nc_testutils::{FakeFolder, FileInfo, FileModifier};

#[test]
fn test_delete_directory_with_new_file() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    // Remove a directory on the server with new files on the client
    fake_folder.remote_modifier().remove("A");
    fake_folder.local_modifier().insert("A/hello.txt", 64, b'W');

    // Symmetry
    fake_folder.local_modifier().remove("B");
    fake_folder
        .remote_modifier()
        .insert("B/hello.txt", 64, b'W');

    assert!(fake_folder.sync_once());

    // A/a1 must be gone because the directory was removed on the server, but hello.txt must be there
    assert!(fake_folder.current_remote_state().find("A/a1").is_none());
    assert!(
        fake_folder
            .current_remote_state()
            .find("A/hello.txt")
            .is_none()
    );

    // Symmetry
    assert!(fake_folder.current_remote_state().find("B/b1").is_none());
    assert!(
        fake_folder
            .current_remote_state()
            .find("B/hello.txt")
            .is_none()
    );

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn issue1329() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    fake_folder.local_modifier().remove("B");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Add a directory that was just removed in the previous sync:
    fake_folder.local_modifier().mkdir("B");
    fake_folder.local_modifier().insert("B/b1", 64, b'W');
    assert!(fake_folder.sync_once());
    assert!(fake_folder.current_remote_state().find("B/b1").is_some());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}
