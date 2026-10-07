// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testuploadreset.cpp (nextcloud/desktop v34.0.5).

use nc_journal::journal::UploadInfo;
use nc_sync::utility::current_secs_since_epoch;
use nc_testutils::{FakeFolder, FileInfo, FileModifier, from_secs, set_temp_root};

// Verify that the chunked transfer eventually gets reset with the new chunking
#[test]
fn test_file_upload_ng() {
    // Big local files go to the target directory, not to /tmp.
    set_temp_root(env!("CARGO_TARGET_TMPDIR"));
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    fake_folder.set_capabilities(serde_json::json!({ "dav": {
        "chunking": "1.0",
        "httpErrorCodesThatResetFailingChunkedUploads": [500] } }));

    let size: i64 = 200 * 1024 * 1024; // 200 MiB
    fake_folder.local_modifier().insert("A/a0", size, b'W');
    let mod_time = current_secs_since_epoch();
    fake_folder
        .local_modifier()
        .set_mod_time("A/a0", from_secs(mod_time));

    // Create a transfer id, so we can make the final MOVE fail
    let mut upload_info = UploadInfo {
        transferid: 1,
        valid: true,
        modtime: mod_time,
        size,
        ..UploadInfo::default()
    };
    fake_folder
        .sync_journal()
        .set_upload_info("A/a0", &upload_info);

    fake_folder.upload_state().mkdir("1");
    fake_folder.server_error_paths().append("1/.file", 500);

    assert!(!fake_folder.sync_once());

    upload_info = fake_folder.sync_journal().get_upload_info("A/a0");
    assert_eq!(upload_info.error_count, 1);
    assert_eq!(upload_info.transferid, 1);

    assert_ne!(fake_folder.sync_journal().wipe_error_blacklist(), 0);
    assert!(!fake_folder.sync_once());

    upload_info = fake_folder.sync_journal().get_upload_info("A/a0");
    assert_eq!(upload_info.error_count, 2);
    assert_eq!(upload_info.transferid, 1);

    assert_ne!(fake_folder.sync_journal().wipe_error_blacklist(), 0);
    assert!(!fake_folder.sync_once());

    upload_info = fake_folder.sync_journal().get_upload_info("A/a0");
    assert_eq!(upload_info.error_count, 3);
    assert_eq!(upload_info.transferid, 1);

    assert_ne!(fake_folder.sync_journal().wipe_error_blacklist(), 0);
    assert!(!fake_folder.sync_once());

    upload_info = fake_folder.sync_journal().get_upload_info("A/a0");
    assert_eq!(upload_info.error_count, 0);
    assert_eq!(upload_info.transferid, 0);
    assert!(!upload_info.valid);
}
