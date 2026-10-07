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

// Checks whether downloads with bad checksums are accepted
#[test]
fn test_checksum_validation() {
    use std::sync::{Arc, Mutex};
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    #[derive(Default)]
    struct Values {
        checksum_value: Option<Vec<u8>>,
        checksum_value_recalculated: Vec<u8>,
        content_md5_value: Option<Vec<u8>>,
        is_checksum_recalculate_supported: bool,
    }
    let values = Arc::new(Mutex::new(Values::default()));
    {
        let values = values.clone();
        fake_folder.set_server_override(move |req, state| {
            let v = values.lock().unwrap();
            let file = nc_testutils::server::file_path_from_url(req.uri()).unwrap_or_default();
            if req.method() == http::Method::GET {
                let mut reply = nc_testutils::server::get_reply(&state.remote_root, &file);
                if let Some(c) = &v.checksum_value {
                    reply
                        .headers_mut()
                        .insert("OC-Checksum", http::HeaderValue::from_bytes(c).unwrap());
                }
                if let Some(c) = &v.content_md5_value {
                    reply
                        .headers_mut()
                        .insert("Content-MD5", http::HeaderValue::from_bytes(c).unwrap());
                }
                return Some(reply.into());
            } else if req.headers().contains_key("X-Recalculate-Hash") {
                if !v.is_checksum_recalculate_supported {
                    return Some(
                        nc_testutils::server::error_reply(402, bytes::Bytes::new()).into(),
                    );
                }
                let mut reply = nc_testutils::server::get_reply(&state.remote_root, &file);
                reply.headers_mut().insert(
                    "OC-Checksum",
                    http::HeaderValue::from_bytes(&v.checksum_value_recalculated).unwrap(),
                );
                return Some(reply.into());
            }
            None
        });
    }
    let set = |f: &dyn Fn(&mut Values)| f(&mut values.lock().unwrap());

    // Basic case
    fake_folder.remote_modifier().create("A/a3", 16, b'A');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Bad OC-Checksum
    set(&|v| v.checksum_value = Some(b"SHA1:bad".to_vec()));
    fake_folder.remote_modifier().create("A/a4", 16, b'A');
    assert!(!fake_folder.sync_once());

    let matched_sha1_checksum = b"SHA1:19b1928d58a2030d08023f3d7054516dbc186f20".to_vec();
    let mismatched_sha1_checksum =
        matched_sha1_checksum[..matched_sha1_checksum.len() - 1].to_vec();

    // Good OC-Checksum
    {
        let m = matched_sha1_checksum.clone(); // printf 'A%.0s' {1..16} | sha1sum -
        set(&move |v| v.checksum_value = Some(m.clone()));
    }
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    set(&|v| v.checksum_value = None);

    // Bad Content-MD5
    set(&|v| v.content_md5_value = Some(b"bad".to_vec()));
    fake_folder.remote_modifier().create("A/a5", 16, b'A');
    assert!(!fake_folder.sync_once());

    // Good Content-MD5
    set(&|v| v.content_md5_value = Some(b"d8a73157ce10cd94a91c2079fc9a92c8".to_vec())); // printf 'A%.0s' {1..16} | md5sum -
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Invalid OC-Checksum is ignored
    set(&|v| v.checksum_value = Some(b"garbage".to_vec()));
    // contentMd5Value is still good
    fake_folder.remote_modifier().create("A/a6", 16, b'A');
    assert!(fake_folder.sync_once());
    set(&|v| v.content_md5_value = Some(b"bad".to_vec()));
    fake_folder.remote_modifier().create("A/a7", 16, b'A');
    assert!(!fake_folder.sync_once());
    set(&|v| v.content_md5_value = Some(Vec::new()));
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // OC-Checksum contains Unsupported checksums
    set(&|v| v.checksum_value = Some(b"Unsupported:XXXX SHA1:invalid Invalid:XxX".to_vec()));
    fake_folder.remote_modifier().create("A/a8", 16, b'A');
    assert!(!fake_folder.sync_once()); // Since the supported SHA1 checksum is invalid, no download
    set(&|v| {
        v.checksum_value = Some(
            b"Unsupported:XXXX SHA1:19b1928d58a2030d08023f3d7054516dbc186f20 Invalid:XxX".to_vec(),
        )
    });
    assert!(fake_folder.sync_once()); // The supported SHA1 checksum is valid now, so the file are downloaded
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Begin Test mismatch recalculation
    let prev_server_version = fake_folder.account().server_version();
    fake_folder.account().set_server_version("24.0.0");

    // Mismatched OC-Checksum and X-Recalculate-Hash is not supported -> sync must fail
    {
        let (mm, m) = (
            mismatched_sha1_checksum.clone(),
            matched_sha1_checksum.clone(),
        );
        set(&move |v| {
            v.is_checksum_recalculate_supported = false;
            v.checksum_value = Some(mm.clone());
            v.checksum_value_recalculated = m.clone();
        });
    }
    fake_folder.remote_modifier().create("A/a9", 16, b'A');
    assert!(!fake_folder.sync_once());

    // Mismatched OC-Checksum and X-Recalculate-Hash is supported, but, recalculated checksum is again mismatched -> sync must fail
    {
        let mm = mismatched_sha1_checksum.clone();
        set(&move |v| {
            v.is_checksum_recalculate_supported = true;
            v.checksum_value = Some(mm.clone());
            v.checksum_value_recalculated = mm.clone();
        });
    }
    assert!(!fake_folder.sync_once());

    // Mismatched OC-Checksum and X-Recalculate-Hash is supported, and, recalculated checksum is a match -> sync must succeed
    {
        let (mm, m) = (
            mismatched_sha1_checksum.clone(),
            matched_sha1_checksum.clone(),
        );
        set(&move |v| {
            v.is_checksum_recalculate_supported = true;
            v.checksum_value = Some(mm.clone());
            v.checksum_value_recalculated = m.clone();
        });
    }
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    set(&|v| v.checksum_value = None);

    fake_folder
        .account()
        .set_server_version(&prev_server_version);
    // End Test mismatch recalculation
}

// Tests the behavior of invalid filename detection
#[test]
fn test_invalid_filename_regex() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    // For current servers, no characters are forbidden
    fake_folder.account().set_server_version("10.0.0");
    fake_folder
        .local_modifier()
        .insert("A/\\:?*\"<>|.txt", 64, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // For legacy servers, some characters were forbidden by the client
    fake_folder.account().set_server_version("8.0.0");
    fake_folder
        .local_modifier()
        .insert("B/\\:?*\"<>|.txt", 64, b'W');
    assert!(fake_folder.sync_once());
    assert!(
        fake_folder
            .current_remote_state()
            .find("B/\\:?*\"<>|.txt")
            .is_none()
    );

    // We can override that by setting the capability
    fake_folder.set_capabilities(serde_json::json!({ "dav": { "invalidFilenameRegex": "" } }));
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Check that new servers also accept the capability
    fake_folder.account().set_server_version("10.0.0");
    fake_folder
        .set_capabilities(serde_json::json!({ "dav": { "invalidFilenameRegex": "my[fgh]ile" } }));
    fake_folder
        .local_modifier()
        .insert("C/myfile.txt", 64, b'W');
    assert!(fake_folder.sync_once());
    assert!(
        fake_folder
            .current_remote_state()
            .find("C/myfile.txt")
            .is_none()
    );
}

#[test]
fn test_discovery_hidden_file() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // We can't depend on currentLocalState for hidden files since
    // it should rightfully skip things like download temporaries
    let local_file_exists = |f: &FakeFolder, name: &str| {
        std::path::Path::new(&format!("{}{}", f.local_path(), name)).exists()
    };

    fake_folder.sync_engine().set_ignore_hidden_files(true);
    fake_folder.remote_modifier().insert("A/.hidden", 64, b'W');
    fake_folder.local_modifier().insert("B/.hidden", 64, b'W');
    assert!(fake_folder.sync_once());
    assert!(!local_file_exists(&fake_folder, "A/.hidden"));
    assert!(
        fake_folder
            .current_remote_state()
            .find("B/.hidden")
            .is_none()
    );

    fake_folder.sync_engine().set_ignore_hidden_files(false);
    fake_folder
        .sync_journal()
        .force_remote_discovery_next_sync();
    assert!(fake_folder.sync_once());
    assert!(local_file_exists(&fake_folder, "A/.hidden"));
    assert!(
        fake_folder
            .current_remote_state()
            .find("B/.hidden")
            .is_some()
    );
}

/// Adapted: only the UTF-8 locale part. The port always uses UTF-8 file
/// names (no `QTextCodec::setCodecForLocale`).
#[test]
fn test_no_local_encoding() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Utf8 locale can sync both
    fake_folder.remote_modifier().insert("A/tößt", 64, b'W');
    fake_folder.remote_modifier().insert("A/t𠜎t", 64, b'W');
    assert!(fake_folder.sync_once());
    assert!(fake_folder.current_local_state().find("A/tößt").is_some());
    assert!(fake_folder.current_local_state().find("A/t𠜎t").is_some());
}

// Aborting has had bugs when there are parallel upload jobs
#[test]
fn test_upload_v1_multiabort() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    let mut options = nc_sync::SyncOptions::default();
    options.initial_chunk_size = 10;
    options.set_max_chunk_size(10);
    options.set_min_chunk_size(10);
    options.minimum_file_age_for_upload = std::time::Duration::ZERO;
    fake_folder.sync_engine().set_sync_options(options);

    let n_put = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let n = n_put.clone();
        fake_folder.set_server_override(move |req, _| {
            if req.method() == http::Method::PUT {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Some(nc_testutils::FakeReply::Hang);
            }
            None
        });
    }

    fake_folder.local_modifier().insert("file", 100, b'W');
    assert!(!fake_folder.sync_once_aborting_after(std::time::Duration::from_millis(100)));

    assert_eq!(n_put.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
fn test_propagate_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    // QFileDevice::Permission(0x7704): owner/user rwx, other r
    let perm = 0o704;
    let set =
        |p: String| std::fs::set_permissions(p, std::fs::Permissions::from_mode(perm)).unwrap();
    set(format!("{}A/a1", fake_folder.local_path()));
    set(format!("{}A/a2", fake_folder.local_path()));
    fake_folder.sync_once(); // get the metadata-only change out of the way
    fake_folder.remote_modifier().append_byte("A/a1");
    fake_folder.remote_modifier().append_byte("A/a2");
    fake_folder.local_modifier().append_byte("A/a2");
    fake_folder.local_modifier().append_byte("A/a2");
    fake_folder.sync_once(); // perms should be preserved
    let mode = |p: String| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(format!("{}A/a1", fake_folder.local_path())), perm);
    assert_eq!(mode(format!("{}A/a2", fake_folder.local_path())), perm);

    let paths = fake_folder.sync_journal().conflict_record_paths();
    let conflict_name = fake_folder.sync_journal().conflict_record(&paths[0]).path;
    let conflict_name = String::from_utf8(conflict_name).unwrap();
    assert!(conflict_name.contains("A/a2"));
    assert_eq!(
        mode(format!("{}{}", fake_folder.local_path(), conflict_name)),
        perm
    );
}

#[test]
fn test_empty_local_but_has_remote() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().mkdir("foo");

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(fake_folder.current_local_state().find("foo").is_some());
}

// Check that server mtime is set on directories on initial propagation
#[test]
fn test_directory_initial_mtime() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().mkdir("foo");
    fake_folder.remote_modifier().insert("foo/bar", 64, b'W');
    let datetime = nc_testutils::from_secs(nc_sync::utility::current_secs_since_epoch()); // wipe ms
    fake_folder
        .remote_modifier()
        .find_mut("foo")
        .unwrap()
        .last_modified = datetime;

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let mtime = std::fs::metadata(format!("{}foo", fake_folder.local_path()))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(mtime, datetime);
}

// A local file should not be modified after upload to server if nothing has changed.
#[test]
fn test_local_file_initial_mtime() {
    use std::os::unix::fs::MetadataExt;
    let foo_folder = "foo/";
    let bar_file = "foo/bar";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.local_modifier().mkdir(foo_folder);
    fake_folder.local_modifier().insert(bar_file, 64, b'W');

    let local_file = fake_folder.local_modifier().find(bar_file);
    let ctime = |p: &std::path::Path| {
        let m = std::fs::metadata(p).unwrap();
        (m.ctime(), m.ctime_nsec())
    };
    let expected_mtime = ctime(&local_file);

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let current_mtime = ctime(&local_file);
    assert_eq!(current_mtime, expected_mtime);
}

fn remote_move_failed_local_move_rolled_back(error_code: u16) {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    // create a big shared folder with some files
    fake_folder.remote_modifier().mkdir("big_shared_folder");
    fake_folder
        .remote_modifier()
        .mkdir("big_shared_folder/shared_files");
    fake_folder.remote_modifier().insert(
        "big_shared_folder/shared_files/big_shared_file_A.data",
        1000,
        b'W',
    );
    fake_folder.remote_modifier().insert(
        "big_shared_folder/shared_files/big_shared_file_B.data",
        1000,
        b'W',
    );

    // make sure big shared folder is synced
    assert!(fake_folder.sync_once());
    assert!(
        fake_folder
            .current_local_state()
            .find("big_shared_folder/shared_files/big_shared_file_A.data")
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find("big_shared_folder/shared_files/big_shared_file_B.data")
            .is_some()
    );
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // try to move from a big shared folder to your own folder
    fake_folder.local_modifier().mkdir("own_folder");
    fake_folder.local_modifier().rename(
        "big_shared_folder/shared_files/big_shared_file_A.data",
        "own_folder/big_shared_file_A.data",
    );
    fake_folder.local_modifier().rename(
        "big_shared_folder/shared_files/big_shared_file_B.data",
        "own_folder/big_shared_file_B.data",
    );

    // emulate server MOVE error
    fake_folder.set_server_override(move |req, _| {
        if req.method().as_str() == "MOVE" {
            return Some(nc_testutils::server::error_reply(error_code, bytes::Bytes::new()).into());
        }
        None
    });

    // make sure the first sync fails and files get restored to original folder
    assert!(!fake_folder.sync_once());

    assert!(fake_folder.sync_once());

    assert!(
        fake_folder
            .current_local_state()
            .find("big_shared_folder/shared_files/big_shared_file_A.data")
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find("big_shared_folder/shared_files/big_shared_file_B.data")
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find("own_folder/big_shared_file_A.data")
            .is_none()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find("own_folder/big_shared_file_B.data")
            .is_none()
    );

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_remote_move_failed_insufficient_storage_local_move_rolled_back() {
    remote_move_failed_local_move_rolled_back(507);
}

#[test]
fn test_remote_move_failed_forbidden_local_move_rolled_back() {
    remote_move_failed_local_move_rolled_back(403);
}

#[test]
fn test_folder_with_files_in_error() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.set_server_override(|req, _| {
        if req.method() == http::Method::GET {
            let file_name = nc_testutils::server::file_path_from_url(req.uri()).unwrap_or_default();
            if file_name == "aaa/subfolder/foo" {
                return Some(nc_testutils::server::error_reply(403, bytes::Bytes::new()).into());
            }
        }
        None
    });

    fake_folder.remote_modifier().mkdir("aaa");
    fake_folder.remote_modifier().mkdir("aaa/subfolder");
    fake_folder
        .remote_modifier()
        .insert("aaa/subfolder/bar", 64, b'W');

    assert!(fake_folder.sync_once());

    fake_folder
        .remote_modifier()
        .insert("aaa/subfolder/foo", 64, b'W');
    assert!(!fake_folder.sync_once());

    assert!(!fake_folder.sync_once());
}

const INVALID_MTIME: i64 = 0;
const CURRENT_MTIME: i64 = 1646057277;

#[test]
fn test_invalid_mtime_recovery_at_start() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let t = nc_testutils::from_secs;

    fake_folder.remote_modifier().insert("foo", 64, b'W');
    fake_folder.remote_modifier().insert("bar", 64, b'W');
    fake_folder.remote_modifier().mkdir("subfolder");
    fake_folder
        .remote_modifier()
        .insert("subfolder/foo", 64, b'W');
    fake_folder
        .remote_modifier()
        .insert("subfolder/bar", 64, b'W');
    fake_folder.remote_modifier().mkdir("aaa");
    fake_folder.remote_modifier().mkdir("aaa/subfolder");
    fake_folder
        .remote_modifier()
        .insert("aaa/subfolder/foo", 64, b'W');
    fake_folder
        .remote_modifier()
        .set_mod_time("aaa/subfolder/foo", t(INVALID_MTIME));
    fake_folder
        .remote_modifier()
        .insert("aaa/subfolder/bar", 64, b'W');
    fake_folder
        .remote_modifier()
        .set_mod_time("aaa/subfolder/bar", t(INVALID_MTIME));

    assert!(!fake_folder.sync_once());

    assert!(!fake_folder.sync_once());

    fake_folder
        .remote_modifier()
        .set_mod_time_keep_etag("aaa/subfolder/foo", t(CURRENT_MTIME));
    fake_folder
        .remote_modifier()
        .set_mod_time_keep_etag("aaa/subfolder/bar", t(CURRENT_MTIME));

    assert!(fake_folder.sync_once());

    assert!(fake_folder.sync_once());

    let expected_state = fake_folder.current_local_state();
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

#[test]
fn test_invalid_mtime_recovery() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let t = nc_testutils::from_secs;

    fake_folder.remote_modifier().insert("foo", 64, b'W');
    fake_folder.remote_modifier().insert("bar", 64, b'W');
    fake_folder.remote_modifier().mkdir("subfolder");
    fake_folder
        .remote_modifier()
        .insert("subfolder/foo", 64, b'W');
    fake_folder
        .remote_modifier()
        .insert("subfolder/bar", 64, b'W');
    fake_folder.remote_modifier().mkdir("aaa");
    fake_folder.remote_modifier().mkdir("aaa/subfolder");
    fake_folder
        .remote_modifier()
        .insert("aaa/subfolder/foo", 64, b'W');
    fake_folder
        .remote_modifier()
        .insert("aaa/subfolder/bar", 64, b'W');

    assert!(fake_folder.sync_once());

    fake_folder
        .remote_modifier()
        .set_mod_time("aaa/subfolder/foo", t(INVALID_MTIME));
    fake_folder
        .remote_modifier()
        .set_mod_time("aaa/subfolder/bar", t(INVALID_MTIME));

    assert!(!fake_folder.sync_once());

    assert!(!fake_folder.sync_once());

    fake_folder
        .remote_modifier()
        .set_mod_time_keep_etag("aaa/subfolder/foo", t(CURRENT_MTIME));
    fake_folder
        .remote_modifier()
        .set_mod_time_keep_etag("aaa/subfolder/bar", t(CURRENT_MTIME));

    assert!(fake_folder.sync_once());

    assert!(fake_folder.sync_once());

    let expected_state = fake_folder.current_local_state();
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

#[test]
fn test_local_invalid_mtime_correction() {
    let t = nc_testutils::from_secs;
    let invalid_mtime = t(0);
    let recent_mtime = t(1743004783); // 2025-03-26T16:59:43+0100

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.local_modifier().insert("invalid", 64, b'W');
    fake_folder
        .local_modifier()
        .set_mod_time("invalid", invalid_mtime);
    fake_folder.local_modifier().insert("recent", 64, b'W');
    fake_folder
        .local_modifier()
        .set_mod_time("recent", recent_mtime);

    assert!(fake_folder.sync_once());

    // "invalid" file had a mtime of 0, so it's been updated to the current time during testing
    let current_mtime = fake_folder
        .current_local_state()
        .find("invalid")
        .unwrap()
        .last_modified;
    assert!(current_mtime > recent_mtime);
    assert!(
        fake_folder
            .current_remote_state()
            .find("invalid")
            .unwrap()
            .last_modified
            > recent_mtime
    );

    // "recent" file had a mtime of RECENT_MTIME, so it shouldn't have been changed
    assert_eq!(
        fake_folder
            .current_local_state()
            .find("recent")
            .unwrap()
            .last_modified,
        recent_mtime
    );
    assert_eq!(
        fake_folder
            .current_remote_state()
            .find("recent")
            .unwrap()
            .last_modified,
        recent_mtime
    );

    assert!(fake_folder.sync_once());

    // verify that the mtime of "invalid" hasn't changed since the last sync that fixed it
    assert_eq!(
        fake_folder
            .current_local_state()
            .find("invalid")
            .unwrap()
            .last_modified,
        current_mtime
    );

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_server_updating_mtime_should_not_create_conflicts() {
    use nc_sync::discovery::LocalDiscoveryStyle;
    use nc_testutils::print_db_data;
    let test_file = "test.txt";
    let t = nc_testutils::from_secs;

    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.remote_modifier().insert(test_file, 64, b'W');
    fake_folder
        .remote_modifier()
        .set_mod_time_keep_etag(test_file, t(CURRENT_MTIME - 2));

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());
    let local_state = fake_folder.current_local_state();
    assert_eq!(local_state, fake_folder.current_remote_state());
    assert_eq!(
        print_db_data(&fake_folder.db_state()),
        print_db_data(&fake_folder.current_remote_state())
    );
    let file_first_sync = local_state.find(test_file).expect("file");
    assert_eq!(file_first_sync.last_modified, t(CURRENT_MTIME - 2));

    fake_folder
        .remote_modifier()
        .set_mod_time_keep_etag(test_file, t(CURRENT_MTIME - 1));

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(fake_folder.sync_once());
    let local_state = fake_folder.current_local_state();
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(
        print_db_data(&fake_folder.db_state()),
        print_db_data(&fake_folder.current_remote_state())
    );
    let file_second_sync = local_state.find(test_file).expect("file");
    assert_eq!(file_second_sync.last_modified, t(CURRENT_MTIME - 1));

    fake_folder
        .remote_modifier()
        .set_mod_time(test_file, t(CURRENT_MTIME));

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(fake_folder.sync_once());
    let local_state = fake_folder.current_local_state();
    assert_eq!(local_state, fake_folder.current_remote_state());
    assert_eq!(
        print_db_data(&fake_folder.db_state()),
        print_db_data(&fake_folder.current_remote_state())
    );
    let file_third_sync = local_state.find(test_file).expect("file");
    assert_eq!(file_third_sync.last_modified, t(CURRENT_MTIME));
}

fn folder_removal_with_case_clash(move_to_trash_enabled: bool) {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    let mut sync_options = fake_folder.sync_engine().sync_options().clone();
    sync_options.move_files_to_trash = move_to_trash_enabled;
    fake_folder.sync_engine().set_sync_options(sync_options);

    fake_folder.remote_modifier().mkdir("A");
    fake_folder.remote_modifier().mkdir("toDelete");
    fake_folder.remote_modifier().insert("A/file", 64, b'W');

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.remote_modifier().insert("A/FILE", 64, b'W');
    assert!(fake_folder.sync_once());

    fake_folder.remote_modifier().mkdir("a");
    fake_folder.remote_modifier().remove("toDelete");

    assert!(fake_folder.sync_once());
    let folder_a = fake_folder.current_local_state().find("toDelete").cloned();
    assert!(folder_a.is_none());
}

#[test]
fn test_folder_removal_with_case_clash() {
    // _data rows "move to trash" and "delete"
    folder_removal_with_case_clash(true);
    folder_removal_with_case_clash(false);
}

// On Linux there is no case clash (case-sensitive file system).
const SHOULD_HAVE_CASE_CLASH_CONFLICT: bool = false;

#[test]
fn test_server_case_clash_create_conflict() {
    use nc_sync::discovery::LocalDiscoveryStyle;
    let test_lower_case_file = "test";
    let test_upper_case_file = "TEST";

    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder
        .remote_modifier()
        .insert("otherFile.txt", 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(test_lower_case_file, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(test_upper_case_file, 64, b'W');

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts = find_case_clash_conflicts(&fake_folder.current_local_state());
    assert_eq!(
        conflicts.len(),
        if SHOULD_HAVE_CASE_CLASH_CONFLICT {
            1
        } else {
            0
        }
    );
    let has_conflict = expect_conflict(&fake_folder.current_local_state(), test_lower_case_file);
    assert_eq!(has_conflict, SHOULD_HAVE_CASE_CLASH_CONFLICT);

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts = find_case_clash_conflicts(&fake_folder.current_local_state());
    assert_eq!(
        conflicts.len(),
        if SHOULD_HAVE_CASE_CLASH_CONFLICT {
            1
        } else {
            0
        }
    );
}

#[test]
fn test_server_sub_folder_case_clash_create_conflict() {
    use nc_sync::discovery::LocalDiscoveryStyle;
    let test_lower_case_file = "a/b/test";
    let test_upper_case_file = "a/b/TEST";

    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.remote_modifier().mkdir("a");
    fake_folder.remote_modifier().mkdir("a/b");
    fake_folder
        .remote_modifier()
        .insert("a/b/otherFile.txt", 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(test_lower_case_file, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(test_upper_case_file, 64, b'W');

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts =
        find_case_clash_conflicts(fake_folder.current_local_state().find("a/b").unwrap());
    assert_eq!(
        conflicts.len(),
        if SHOULD_HAVE_CASE_CLASH_CONFLICT {
            1
        } else {
            0
        }
    );
    let has_conflict = expect_conflict(&fake_folder.current_local_state(), test_lower_case_file);
    assert_eq!(has_conflict, SHOULD_HAVE_CASE_CLASH_CONFLICT);

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts =
        find_case_clash_conflicts(fake_folder.current_local_state().find("a/b").unwrap());
    assert_eq!(
        conflicts.len(),
        if SHOULD_HAVE_CASE_CLASH_CONFLICT {
            1
        } else {
            0
        }
    );
}

#[test]
fn test_server_case_clash_create_conflict_on_move() {
    use nc_sync::discovery::LocalDiscoveryStyle;
    let test_lower_case_file = "test";
    let test_upper_case_file = "TEST2";
    let test_upper_case_file_after_move = "TEST";

    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder
        .remote_modifier()
        .insert("otherFile.txt", 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(test_lower_case_file, 64, b'W');
    fake_folder
        .remote_modifier()
        .insert(test_upper_case_file, 64, b'W');

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts = find_case_clash_conflicts(&fake_folder.current_local_state());
    assert_eq!(conflicts.len(), 0);
    let has_conflict = expect_conflict(&fake_folder.current_local_state(), test_lower_case_file);
    assert!(!has_conflict);

    fake_folder
        .remote_modifier()
        .rename(test_upper_case_file, test_upper_case_file_after_move);

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts = find_case_clash_conflicts(&fake_folder.current_local_state());
    assert_eq!(
        conflicts.len(),
        if SHOULD_HAVE_CASE_CLASH_CONFLICT {
            1
        } else {
            0
        }
    );
    let has_conflict_after_move = expect_conflict(
        &fake_folder.current_local_state(),
        test_upper_case_file_after_move,
    );
    assert_eq!(has_conflict_after_move, SHOULD_HAVE_CASE_CLASH_CONFLICT);

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    let conflicts = find_case_clash_conflicts(&fake_folder.current_local_state());
    assert_eq!(
        conflicts.len(),
        if SHOULD_HAVE_CASE_CLASH_CONFLICT {
            1
        } else {
            0
        }
    );
}
