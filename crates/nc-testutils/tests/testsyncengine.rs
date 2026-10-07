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

fn db_checksum(fake_folder: &FakeFolder, path: &str) -> Vec<u8> {
    fake_folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .unwrap()
        .unwrap_or_default()
        .checksum_header
}

fn db_record(fake_folder: &FakeFolder, path: &str) -> nc_journal::journal::SyncJournalFileRecord {
    fake_folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .unwrap()
        .unwrap_or_default()
}

#[test]
fn test_eml_local_checksum() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.local_modifier().insert("a1.eml", 64, b'A');
    fake_folder.local_modifier().insert("a2.eml", 64, b'A');
    fake_folder.local_modifier().insert("a3.eml", 64, b'A');
    fake_folder.local_modifier().insert("b3.txt", 64, b'A');
    // Upload and calculate the checksums
    fake_folder.sync_once();

    // printf 'A%.0s' {1..64} | sha1sum -
    let reference_checksum = b"SHA1:30b86e44e6001403827a62c58b08893e77cf121f".to_vec();
    assert_eq!(db_checksum(&fake_folder, "a1.eml"), reference_checksum);
    assert_eq!(db_checksum(&fake_folder, "a2.eml"), reference_checksum);
    assert_eq!(db_checksum(&fake_folder, "a3.eml"), reference_checksum);
    assert_eq!(db_checksum(&fake_folder, "b3.txt"), reference_checksum);

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    // Touch the file without changing the content, shouldn't upload
    fake_folder.local_modifier().set_contents("a1.eml", b'A');
    // Change the content/size
    fake_folder.local_modifier().set_contents("a2.eml", b'B');
    fake_folder.local_modifier().append_byte("a3.eml");
    fake_folder.local_modifier().append_byte("b3.txt");
    fake_folder.sync_once();

    assert_eq!(db_checksum(&fake_folder, "a1.eml"), reference_checksum);
    assert_eq!(
        db_checksum(&fake_folder, "a2.eml"),
        b"SHA1:84951fc23a4dafd10020ac349da1f5530fa65949"
    );
    assert_eq!(
        db_checksum(&fake_folder, "a3.eml"),
        b"SHA1:826b7e7a7af8a529ae1c7443c23bf185c0ad440c"
    );
    assert_eq!(
        db_checksum(&fake_folder, "b3.eml"),
        db_checksum(&fake_folder, "a3.txt")
    );

    assert!(!item_did_complete(&complete_spy, "a1.eml"));
    assert!(item_did_complete_successfully(&complete_spy, "a2.eml"));
    assert!(item_did_complete_successfully(&complete_spy, "a3.eml"));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_selective_sync_bug() {
    // issue owncloud/enterprise#1965: files from selective-sync ignored
    // folders are uploaded anyway is some circumstances.
    let mut fake_folder = FakeFolder::new(FileInfo::dir(
        "",
        [FileInfo::dir(
            "parentFolder",
            [
                FileInfo::dir(
                    "subFolderA",
                    [
                        FileInfo::file("fileA.txt", 400),
                        FileInfo::file_with_char("fileB.txt", 400, b'o'),
                        FileInfo::dir(
                            "subsubFolder",
                            [
                                FileInfo::file("fileC.txt", 400),
                                FileInfo::file_with_char("fileD.txt", 400, b'o'),
                            ],
                        ),
                        FileInfo::dir(
                            "anotherFolder",
                            [
                                FileInfo::dir("emptyFolder", []),
                                FileInfo::dir(
                                    "subsubFolder",
                                    [
                                        FileInfo::file("fileE.txt", 400),
                                        FileInfo::file_with_char("fileF.txt", 400, b'o'),
                                    ],
                                ),
                            ],
                        ),
                    ],
                ),
                FileInfo::dir("subFolderB", []),
            ],
        )],
    ));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let expected_server_state = fake_folder.current_remote_state();

    // Remove subFolderA with selectiveSync:
    fake_folder.sync_journal().set_selective_sync_list(
        nc_journal::journal::SelectiveSyncListType::BlackList,
        &["parentFolder/subFolderA/".to_owned()],
    );
    fake_folder
        .sync_journal()
        .schedule_path_for_remote_discovery(b"parentFolder/subFolderA/");
    let get_etag = |f: &FakeFolder, file: &str| db_record(f, file).etag;
    assert_eq!(get_etag(&fake_folder, "parentFolder"), b"_invalid_");
    assert_eq!(
        get_etag(&fake_folder, "parentFolder/subFolderA"),
        b"_invalid_"
    );
    assert_ne!(
        get_etag(&fake_folder, "parentFolder/subFolderA/subsubFolder"),
        b"_invalid_"
    );

    // But touch local file before the next sync, such that the local folder
    // can't be removed
    fake_folder
        .local_modifier()
        .set_contents("parentFolder/subFolderA/fileB.txt", b'n');
    fake_folder
        .local_modifier()
        .set_contents("parentFolder/subFolderA/subsubFolder/fileD.txt", b'n');
    fake_folder.local_modifier().set_contents(
        "parentFolder/subFolderA/anotherFolder/subsubFolder/fileF.txt",
        b'n',
    );

    // Several follow-up syncs don't change the state at all,
    // in particular the remote state doesn't change and fileB.txt
    // isn't uploaded.
    for _ in 0..3 {
        fake_folder.sync_once();
        // Nothing changed on the server
        assert_eq!(fake_folder.current_remote_state(), expected_server_state);
        // The local state should still have subFolderA
        let local = fake_folder.current_local_state();
        assert!(local.find("parentFolder/subFolderA").is_some());
        assert!(local.find("parentFolder/subFolderA/fileA.txt").is_none());
        assert!(local.find("parentFolder/subFolderA/fileB.txt").is_some());
        assert!(
            local
                .find("parentFolder/subFolderA/subsubFolder/fileC.txt")
                .is_none()
        );
        assert!(
            local
                .find("parentFolder/subFolderA/subsubFolder/fileD.txt")
                .is_some()
        );
        assert!(
            local
                .find("parentFolder/subFolderA/anotherFolder/subsubFolder/fileE.txt")
                .is_none()
        );
        assert!(
            local
                .find("parentFolder/subFolderA/anotherFolder/subsubFolder/fileF.txt")
                .is_some()
        );
        assert!(
            local
                .find("parentFolder/subFolderA/anotherFolder/emptyFolder")
                .is_none()
        );
        assert!(local.find("parentFolder/subFolderB").is_some());
    }
}

#[test]
fn abort_after_failed_mkdir() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    let finished = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    {
        let f = finished.clone();
        fake_folder.sync_engine().callbacks().finished =
            Some(Box::new(move |ok| f.borrow_mut().push(ok)));
    }
    fake_folder.server_error_paths().append("NewFolder", 500);
    fake_folder.local_modifier().mkdir("NewFolder");
    // This should be aborted and would otherwise fail in FileInfo::create.
    fake_folder
        .local_modifier()
        .insert("NewFolder/NewFile", 64, b'W');
    fake_folder.sync_once();
    assert_eq!(finished.borrow().len(), 1);
    assert!(!finished.borrow()[0]);
}

/// Verify that an incompletely propagated directory doesn't have the server's
/// etag stored in the database yet.
#[test]
fn test_dir_etag_after_incomplete_sync() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder
        .server_error_paths()
        .append("NewFolder/foo", 500);
    fake_folder.remote_modifier().mkdir("NewFolder");
    fake_folder
        .remote_modifier()
        .insert("NewFolder/foo", 64, b'W');
    assert!(!fake_folder.sync_once());

    let rec = db_record(&fake_folder, "NewFolder");
    assert!(rec.is_valid());
    assert_eq!(rec.etag, b"_invalid_");
    assert!(!rec.file_id.is_empty());
}

#[test]
fn test_dir_download_with_error() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    fake_folder.remote_modifier().mkdir("Y");
    fake_folder.remote_modifier().mkdir("Y/Z");
    for i in 0..10 {
        fake_folder
            .remote_modifier()
            .insert(&format!("Y/Z/d{i}"), 64, b'W');
    }
    fake_folder.server_error_paths().append("Y/Z/d2", 503);
    fake_folder.server_error_paths().append("Y/Z/d3", 503);
    assert!(!fake_folder.sync_once());

    let mut seen = std::collections::HashSet::new();
    for item in complete_spy.items() {
        assert!(!seen.contains(&item.file), "signal only sent once per item");
        seen.insert(item.file.clone());
        if item.file == "Y/Z/d2" {
            assert_eq!(item.status, nc_sync::Status::NormalError);
        } else if item.file == "Y/Z/d3" {
            assert_ne!(item.status, nc_sync::Status::Success);
        } else if !item.is_directory() {
            assert_eq!(item.status, nc_sync::Status::Success);
        }
    }
}

fn fake_conflict(same_mtime: bool, checksums: &str, expected_get: usize) {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let n_get = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let n = n_get.clone();
        fake_folder.set_server_override(move |req, _| {
            if req.method() == http::Method::GET {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            None
        });
    }
    // Base mtime with no ms content (filesystem is seconds only)
    let secs = nc_sync::utility::current_secs_since_epoch() - 4 * 24 * 3600;
    let mut mtime = nc_testutils::from_secs(secs);
    fake_folder.local_modifier().set_contents("A/a1", b'C');
    fake_folder.local_modifier().set_mod_time("A/a1", mtime);
    fake_folder.remote_modifier().set_contents("A/a1", b'C');
    if !same_mtime {
        mtime += std::time::Duration::from_secs(24 * 3600);
    }
    fake_folder.remote_modifier().set_mod_time("A/a1", mtime);
    fake_folder
        .remote_modifier()
        .find_mut("A/a1")
        .unwrap()
        .checksums = checksums.to_owned();
    assert!(fake_folder.sync_once());
    assert_eq!(
        n_get.load(std::sync::atomic::Ordering::SeqCst),
        expected_get
    );

    // check that mtime in journal and filesystem agree
    let a1path = format!("{}A/a1", fake_folder.local_path());
    let a1record = db_record(&fake_folder, "A/a1");
    assert_eq!(a1record.modtime, nc_sync::filesystem::get_mod_time(&a1path));

    // Extra sync reads from db, no difference
    assert!(fake_folder.sync_once());
    assert_eq!(
        n_get.load(std::sync::atomic::Ordering::SeqCst),
        expected_get
    );
}

#[test]
fn test_fake_conflict() {
    // Same mtime, but no server checksum -> ignored in reconcile
    fake_conflict(true, "", 1);
    // Same mtime, weak server checksum differ -> downloaded
    fake_conflict(true, "Adler32:bad", 1);
    // Same mtime, matching weak checksum -> skipped
    fake_conflict(true, "Adler32:2a2010d", 0);
    // Same mtime, strong server checksum differ -> downloaded
    fake_conflict(true, "SHA1:bad", 1);
    // Same mtime, matching strong checksum -> skipped
    fake_conflict(true, "SHA1:56900fb1d337cf7237ff766276b9c1e8ce507427", 0);
    // mtime changed, but no server checksum -> download
    fake_conflict(false, "", 1);
    // mtime changed, weak checksum match -> download anyway
    fake_conflict(false, "Adler32:2a2010d", 1);
    // mtime changed, strong checksum match -> skip
    fake_conflict(false, "SHA1:56900fb1d337cf7237ff766276b9c1e8ce507427", 0);
}

/// Checks whether SyncFileItems have the expected properties before start
/// of propagation.
#[test]
fn test_sync_file_item_properties() {
    let now = nc_sync::utility::current_secs_since_epoch();
    let initial_mtime = now - 7 * 24 * 3600;
    let changed_mtime = now - 4 * 24 * 3600;
    let changed_mtime2 = now - 3 * 24 * 3600;
    let t = nc_testutils::from_secs;

    // Ensure the initial mtimes are as expected
    let mut initial_file_info = FileInfo::A12_B12_C12_S12();
    initial_file_info.set_mod_time("A/a1", t(initial_mtime));
    initial_file_info.set_mod_time("B/b1", t(initial_mtime));
    initial_file_info.set_mod_time("C/c1", t(initial_mtime));

    let mut fake_folder = FakeFolder::new(initial_file_info);

    // upload a
    fake_folder.local_modifier().append_byte("A/a1");
    fake_folder
        .local_modifier()
        .set_mod_time("A/a1", t(changed_mtime));
    // download b
    fake_folder.remote_modifier().append_byte("B/b1");
    fake_folder
        .remote_modifier()
        .set_mod_time("B/b1", t(changed_mtime));
    // conflict c
    fake_folder.local_modifier().append_byte("C/c1");
    fake_folder.local_modifier().append_byte("C/c1");
    fake_folder
        .local_modifier()
        .set_mod_time("C/c1", t(changed_mtime));
    fake_folder.remote_modifier().append_byte("C/c1");
    fake_folder
        .remote_modifier()
        .set_mod_time("C/c1", t(changed_mtime2));

    let checked = std::rc::Rc::new(std::cell::Cell::new(false));
    {
        let checked = checked.clone();
        fake_folder.sync_engine().callbacks().about_to_propagate = Some(Box::new(move |items| {
            let find = |f: &str| {
                items
                    .iter()
                    .find(|i| i.borrow().file == f)
                    .map(|i| i.borrow().clone())
            };
            use nc_sync::{Direction, Instruction};
            // a1: should have local size and modtime
            let a1 = find("A/a1").expect("a1");
            assert_eq!(a1.instruction, Instruction::Sync);
            assert_eq!(a1.direction, Direction::Up);
            assert_eq!(a1.size, 5);
            assert_eq!(a1.modtime, changed_mtime);
            assert_eq!(a1.previous_size, 4);
            assert_eq!(a1.previous_modtime, initial_mtime);
            // b2: should have remote size and modtime
            let b1 = find("B/b1").expect("b1");
            assert_eq!(b1.instruction, Instruction::Sync);
            assert_eq!(b1.direction, Direction::Down);
            assert_eq!(b1.size, 17);
            assert_eq!(b1.modtime, changed_mtime);
            assert_eq!(b1.previous_size, 16);
            assert_eq!(b1.previous_modtime, initial_mtime);
            // c1: conflicts are downloads, so remote size and modtime
            let c1 = find("C/c1").expect("c1");
            assert_eq!(c1.instruction, Instruction::Conflict);
            assert_eq!(c1.direction, Direction::None);
            assert_eq!(c1.size, 25);
            assert_eq!(c1.modtime, changed_mtime2);
            assert_eq!(c1.previous_size, 26);
            assert_eq!(c1.previous_modtime, changed_mtime);
            checked.set(true);
        }));
    }
    assert!(fake_folder.sync_once());
    assert!(checked.get());
}

/// Checks whether subsequent large uploads are skipped after a 507 error
#[test]
fn test_insufficient_remote_storage() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    // Disable parallel uploads
    let mut sync_options = nc_sync::SyncOptions::default();
    sync_options.parallel_network_jobs = 0;
    sync_options.minimum_file_age_for_upload = std::time::Duration::ZERO;
    fake_folder.sync_engine().set_sync_options(sync_options);

    // Produce an error based on upload size
    let remote_quota = 1000;
    let counters = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize))); // (n507, nPUT)
    {
        let c = counters.clone();
        fake_folder.set_server_override(move |req, _| {
            if req.method() == http::Method::PUT {
                let mut c = c.lock().unwrap();
                c.1 += 1;
                let total: i64 = req
                    .headers()
                    .get("OC-Total-Length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if total > remote_quota {
                    c.0 += 1;
                    return Some(
                        nc_testutils::server::error_reply(507, bytes::Bytes::new()).into(),
                    );
                }
            }
            None
        });
    }

    fake_folder.local_modifier().insert("A/big", 800, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(counters.lock().unwrap().1, 1);
    assert_eq!(counters.lock().unwrap().0, 0);

    counters.lock().unwrap().1 = 0;
    fake_folder.local_modifier().insert("A/big1", 500, b'W'); // ok
    fake_folder.local_modifier().insert("A/big2", 1200, b'W'); // 507 (quota guess now 1199)
    fake_folder.local_modifier().insert("A/big3", 1200, b'W'); // skipped
    fake_folder.local_modifier().insert("A/big4", 1500, b'W'); // skipped
    fake_folder.local_modifier().insert("A/big5", 1100, b'W'); // 507 (quota guess now 1099)
    fake_folder.local_modifier().insert("A/big6", 900, b'W'); // ok (quota guess now 199)
    fake_folder.local_modifier().insert("A/big7", 200, b'W'); // skipped
    fake_folder.local_modifier().insert("A/big8", 199, b'W'); // ok (quota guess now 0)

    fake_folder.local_modifier().insert("B/big8", 1150, b'W'); // 507
    assert!(!fake_folder.sync_once());
    assert_eq!(counters.lock().unwrap().1, 6);
    assert_eq!(counters.lock().unwrap().0, 3);
}
