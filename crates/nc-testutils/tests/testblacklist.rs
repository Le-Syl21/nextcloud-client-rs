// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testblacklist.cpp (nextcloud/desktop v34.0.5).

use std::sync::{Arc, Mutex};

use nc_journal::journal::{ErrorCategory as BlacklistCategory, SyncJournalFileRecord};
use nc_sync::{Instruction, Status, SyncFileItem};
use nc_testutils::{FakeFolder, FileInfo, FileModifier, ItemCompletedSpy};

fn journal_record(folder: &FakeFolder, path: &str) -> SyncJournalFileRecord {
    folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// `completeSpy.findItem(path)` + `QVERIFY(it)`.
fn find_existing_item(spy: &ItemCompletedSpy, path: &str) -> SyncFileItem {
    let item = spy.items().into_iter().find(|i| i.destination() == path);
    assert!(item.is_some(), "no completed item for {path}");
    item.unwrap()
}

/// `auto &modifier = remote ? fakeFolder.remoteModifier() : fakeFolder.localModifier();`
fn with_modifier(
    fake_folder: &mut FakeFolder,
    remote: bool,
    f: impl FnOnce(&mut dyn FileModifier),
) {
    if remote {
        f(&mut *fake_folder.remote_modifier());
    } else {
        f(fake_folder.local_modifier());
    }
}

fn blacklist_basic(remote: bool) {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    let counter = Arc::new(Mutex::new(0));
    let test_file_name = "A/new";
    let req_id = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let counter = counter.clone();
        let req_id = req_id.clone();
        fake_folder.set_server_override(move |req, _state| {
            if req.uri().path().ends_with(test_file_name) {
                *req_id.lock().unwrap() = req
                    .headers()
                    .get("X-Request-ID")
                    .map(|v| v.as_bytes().to_vec())
                    .unwrap_or_default();
            }
            if !remote && req.method() == http::Method::PUT {
                *counter.lock().unwrap() += 1;
            }
            if remote && req.method() == http::Method::GET {
                *counter.lock().unwrap() += 1;
            }
            None
        });
    }

    let cleanup = || complete_spy.clear();

    let initial_etag = journal_record(&fake_folder, "A").etag;
    assert!(!initial_etag.is_empty());

    // The first sync and the download will fail - the item will be blacklisted
    with_modifier(&mut fake_folder, remote, |m| {
        m.insert(test_file_name, 64, b'W')
    });
    fake_folder.server_error_paths().append(test_file_name, 500); // will be blacklisted
    assert!(!fake_folder.sync_once());
    {
        let it = find_existing_item(&complete_spy, test_file_name);
        assert_eq!(it.status, Status::NormalError); // initial error visible
        assert_eq!(it.instruction, Instruction::New);

        let entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        assert!(entry.is_valid());
        assert_eq!(entry.error_category, BlacklistCategory::Normal);
        assert_eq!(entry.retry_count, 1);
        assert_eq!(*counter.lock().unwrap(), 1);
        assert!(entry.ignore_duration > 0);
        assert_eq!(entry.request_id, *req_id.lock().unwrap());

        if remote {
            assert_eq!(journal_record(&fake_folder, "A").etag, initial_etag);
        }
    }
    cleanup();

    // Ignored during the second run - but soft errors are also errors
    assert!(!fake_folder.sync_once());
    {
        let it = find_existing_item(&complete_spy, test_file_name);
        assert_eq!(it.status, Status::BlacklistedError);
        assert_eq!(it.instruction, Instruction::Ignore); // no retry happened!

        let entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        assert!(entry.is_valid());
        assert_eq!(entry.error_category, BlacklistCategory::Normal);
        assert_eq!(entry.retry_count, 1);
        assert_eq!(*counter.lock().unwrap(), 1);
        assert!(entry.ignore_duration > 0);
        assert_eq!(entry.request_id, *req_id.lock().unwrap());

        if remote {
            assert_eq!(journal_record(&fake_folder, "A").etag, initial_etag);
        }
    }
    cleanup();

    // Let's expire the blacklist entry to verify it gets retried
    {
        let mut entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        entry.ignore_duration = 1;
        entry.last_try_time -= 1;
        fake_folder.sync_journal().set_error_blacklist_entry(&entry);
    }
    assert!(!fake_folder.sync_once());
    {
        let it = find_existing_item(&complete_spy, test_file_name);
        assert_eq!(it.status, Status::BlacklistedError); // blacklisted as it's just a retry
        assert_eq!(it.instruction, Instruction::New); // retry!

        let entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        assert!(entry.is_valid());
        assert_eq!(entry.error_category, BlacklistCategory::Normal);
        assert_eq!(entry.retry_count, 2);
        assert_eq!(*counter.lock().unwrap(), 2);
        assert!(entry.ignore_duration > 0);
        assert_eq!(entry.request_id, *req_id.lock().unwrap());

        if remote {
            assert_eq!(journal_record(&fake_folder, "A").etag, initial_etag);
        }
    }
    cleanup();

    // When the file changes a retry happens immediately
    with_modifier(&mut fake_folder, remote, |m| m.append_byte(test_file_name));
    assert!(!fake_folder.sync_once());
    {
        let it = find_existing_item(&complete_spy, test_file_name);
        assert_eq!(it.status, Status::BlacklistedError);
        assert_eq!(it.instruction, Instruction::New); // retry!

        let entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        assert!(entry.is_valid());
        assert_eq!(entry.error_category, BlacklistCategory::Normal);
        assert_eq!(entry.retry_count, 3);
        assert_eq!(*counter.lock().unwrap(), 3);
        assert!(entry.ignore_duration > 0);
        assert_eq!(entry.request_id, *req_id.lock().unwrap());

        if remote {
            assert_eq!(journal_record(&fake_folder, "A").etag, initial_etag);
        }
    }
    cleanup();

    // When the error goes away and the item is retried, the sync succeeds
    fake_folder.server_error_paths().clear();
    {
        let mut entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        entry.ignore_duration = 1;
        entry.last_try_time -= 1;
        fake_folder.sync_journal().set_error_blacklist_entry(&entry);
    }
    assert!(fake_folder.sync_once());
    {
        let it = find_existing_item(&complete_spy, test_file_name);
        assert_eq!(it.status, Status::Success);
        assert_eq!(it.instruction, Instruction::New);

        let entry = fake_folder
            .sync_journal()
            .error_blacklist_entry(test_file_name);
        assert!(!entry.is_valid());
        assert_eq!(*counter.lock().unwrap(), 4);

        if remote {
            assert_eq!(
                String::from_utf8_lossy(&journal_record(&fake_folder, "A").etag),
                fake_folder.current_remote_state().find("A").unwrap().etag
            );
        }
    }
    cleanup();

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_blacklist_basic() {
    // _data row "remote"
    blacklist_basic(true);
    // _data row "local"
    blacklist_basic(false);
}
