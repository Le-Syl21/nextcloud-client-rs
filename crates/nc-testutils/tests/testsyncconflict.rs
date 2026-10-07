// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testsyncconflict.cpp (nextcloud/desktop v34.0.5).
//
// testConflictFileBaseName is ported in crates/nc-journal/tests/utility.rs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use http::HeaderValue;
use nc_journal::journal::{ConflictRecord, SyncJournalFileRecord};
use nc_journal::utility::conflict_file_base_name_from_pattern;
use nc_sync::{AnotherSyncNeeded, Instruction, Status};
use nc_testutils::server::{file_path_from_url, get_reply};
use nc_testutils::{FakeFolder, FileInfo, FileModifier, ItemCompletedSpy, PathComponents};

fn item_successful(spy: &ItemCompletedSpy, path: &str, instr: Instruction) -> bool {
    let item = spy.find_item(path);
    item.status == Status::Success && item.instruction == instr
}

fn item_conflict(spy: &ItemCompletedSpy, path: &str) -> bool {
    let item = spy.find_item(path);
    item.status == Status::Conflict && item.instruction == Instruction::Conflict
}

#[allow(dead_code)]
fn item_successful_move(spy: &ItemCompletedSpy, path: &str) -> bool {
    item_successful(spy, path, Instruction::Rename)
}

fn find_conflicts(dir: &FileInfo) -> Vec<String> {
    let mut conflicts = Vec::new();
    for item in dir.children.values() {
        if item.name.contains("(conflicted copy") {
            conflicts.push(item.path());
        }
    }
    conflicts
}

fn expect_and_wipe_conflict(local: &mut dyn FileModifier, state: FileInfo, path: &str) -> bool {
    let path_components = PathComponents::new(path);
    let Some(base) = state.find(path_components.parent_dir_components()) else {
        return false;
    };
    for item in base.children.values() {
        if item.name.starts_with(path_components.file_name())
            && item.name.contains("(conflicted copy")
        {
            local.remove(&item.path());
            return true;
        }
    }
    false
}

fn db_record(folder: &FakeFolder, path: &str) -> SyncJournalFileRecord {
    folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// `QMap::operator[]` on the children: the child, or an empty `FileInfo`.
fn child(state: &FileInfo, name: &str) -> FileInfo {
    state.children.get(name).cloned().unwrap_or_default()
}

type ConflictMap = Arc<Mutex<BTreeMap<Vec<u8>, String>>>;

/// The override shared by testUploadAfterDownload and testSeparateUpload:
/// records the conflict uploads by base file id.
fn record_conflict_uploads(fake_folder: &FakeFolder) -> ConflictMap {
    let conflict_map: ConflictMap = Arc::default();
    let map = conflict_map.clone();
    fake_folder.set_server_override(move |request, _| {
        if request.method() == http::Method::PUT
            && request
                .headers()
                .get("OC-Conflict")
                .is_some_and(|v| v.as_bytes() == b"1")
        {
            let base_file_id = request
                .headers()
                .get("OC-ConflictBaseFileId")
                .map(|v| v.as_bytes().to_vec())
                .unwrap_or_default();
            // request.url().toString().split('/'), last two components
            let url = file_path_from_url(request.uri()).unwrap_or_default();
            let components: Vec<&str> = url.split('/').collect();
            let conflict_file = components[components.len().saturating_sub(2)..].join("/");
            map.lock()
                .unwrap()
                .insert(base_file_id.clone(), conflict_file.clone());
            assert!(!base_file_id.is_empty());
            assert_eq!(
                request
                    .headers()
                    .get("OC-ConflictInitialBasePath")
                    .map(|v| v.as_bytes().to_vec())
                    .unwrap_or_default(),
                conflict_file_base_name_from_pattern(conflict_file.as_bytes())
            );
        }
        None
    });
    conflict_map
}

fn remote_file_id(fake_folder: &FakeFolder, path: &str) -> Vec<u8> {
    fake_folder
        .remote_modifier()
        .find(path)
        .unwrap()
        .file_id
        .clone()
        .into_bytes()
}

#[test]
fn test_no_upload() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder.local_modifier().set_contents("A/a1", b'L');
    fake_folder.remote_modifier().set_contents("A/a1", b'R');
    fake_folder.local_modifier().append_byte("A/a2");
    fake_folder.remote_modifier().append_byte("A/a2");
    fake_folder.remote_modifier().append_byte("A/a2");
    assert!(fake_folder.sync_once());

    // Verify that the conflict names don't have the user name
    let conflicts = find_conflicts(&child(&fake_folder.current_local_state(), "A"));
    for conflict in &conflicts {
        assert!(!conflict.contains(&fake_folder.account().dav_display_name()));
    }

    let state = fake_folder.current_local_state();
    assert!(expect_and_wipe_conflict(
        fake_folder.local_modifier(),
        state,
        "A/a1"
    ));
    let state = fake_folder.current_local_state();
    assert!(expect_and_wipe_conflict(
        fake_folder.local_modifier(),
        state,
        "A/a2"
    ));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_upload_after_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": true }));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let conflict_map = record_conflict_uploads(&fake_folder);

    fake_folder.local_modifier().set_contents("A/a1", b'L');
    fake_folder.remote_modifier().set_contents("A/a1", b'R');
    fake_folder.local_modifier().append_byte("A/a2");
    fake_folder.remote_modifier().append_byte("A/a2");
    fake_folder.remote_modifier().append_byte("A/a2");
    assert!(fake_folder.sync_once());
    let local = fake_folder.current_local_state();
    let remote = fake_folder.current_remote_state();
    assert_eq!(local, remote);

    let a1_file_id = remote_file_id(&fake_folder, "A/a1");
    let a2_file_id = remote_file_id(&fake_folder, "A/a2");
    let conflict_map = conflict_map.lock().unwrap().clone();
    assert!(conflict_map.contains_key(&a1_file_id));
    assert!(conflict_map.contains_key(&a2_file_id));
    assert_eq!(conflict_map.len(), 2);
    assert_eq!(
        conflict_file_base_name_from_pattern(conflict_map[&a1_file_id].as_bytes()),
        b"A/a1"
    );

    // Check that the conflict file contains the username
    assert!(conflict_map[&a1_file_id].contains(&format!(
        "(conflicted copy {} ",
        fake_folder.account().dav_display_name()
    )));

    assert_eq!(
        remote
            .find(conflict_map[&a1_file_id].as_str())
            .unwrap()
            .content_char,
        b'L'
    );
    assert_eq!(remote.find("A/a1").unwrap().content_char, b'R');

    assert_eq!(
        remote
            .find(conflict_map[&a2_file_id].as_str())
            .unwrap()
            .size,
        5
    );
    assert_eq!(remote.find("A/a2").unwrap().size, 6);
}

#[test]
fn test_separate_upload() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": true }));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let conflict_map = record_conflict_uploads(&fake_folder);

    // Explicitly add a conflict file to simulate the case where the upload of the
    // file didn't finish in the same sync run that the conflict was created.
    // To do that we need to create a mock conflict record.
    let a1_file_id = remote_file_id(&fake_folder, "A/a1");
    let conflict_name = "A/a1 (conflicted copy me 1234)";
    fake_folder.local_modifier().insert(conflict_name, 64, b'L');
    let conflict_record = ConflictRecord {
        path: conflict_name.as_bytes().to_vec(),
        base_file_id: a1_file_id.clone(),
        initial_base_path: b"A/a1".to_vec(),
        ..ConflictRecord::default()
    };
    fake_folder
        .sync_journal()
        .set_conflict_record(&conflict_record);
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(conflict_map.lock().unwrap().len(), 1);
    assert_eq!(conflict_map.lock().unwrap()[&a1_file_id], conflict_name);
    assert_eq!(
        fake_folder
            .current_remote_state()
            .find(conflict_map.lock().unwrap()[&a1_file_id].as_str())
            .unwrap()
            .content_char,
        b'L'
    );
    conflict_map.lock().unwrap().clear();

    // Now the user can locally alter the conflict file and it will be uploaded
    // as usual.
    fake_folder
        .local_modifier()
        .set_contents(conflict_name, b'P');
    assert!(fake_folder.sync_once());
    assert_eq!(conflict_map.lock().unwrap().len(), 1);
    assert_eq!(conflict_map.lock().unwrap()[&a1_file_id], conflict_name);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    conflict_map.lock().unwrap().clear();

    // Similarly, remote modifications of conflict files get propagated downwards
    fake_folder
        .remote_modifier()
        .set_contents(conflict_name, b'Q');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert!(conflict_map.lock().unwrap().is_empty());

    // Conflict files for conflict files!
    let a1_conflict_file_id = remote_file_id(&fake_folder, conflict_name);
    fake_folder.remote_modifier().append_byte(conflict_name);
    fake_folder.remote_modifier().append_byte(conflict_name);
    fake_folder.local_modifier().append_byte(conflict_name);
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(conflict_map.lock().unwrap().len(), 1);
    assert!(
        conflict_map
            .lock()
            .unwrap()
            .contains_key(&a1_conflict_file_id)
    );
    assert_eq!(
        fake_folder
            .current_remote_state()
            .find(conflict_name)
            .unwrap()
            .size,
        66
    );
    assert_eq!(
        fake_folder
            .current_remote_state()
            .find(conflict_map.lock().unwrap()[&a1_conflict_file_id].as_str())
            .unwrap()
            .size,
        65
    );
    conflict_map.lock().unwrap().clear();
}

// What happens if we download a conflict file? Is the metadata set up correctly?
#[test]
fn test_downloading_conflict_file() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": true }));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // With no headers from the server
    fake_folder
        .remote_modifier()
        .insert("A/a1 (conflicted copy 1234)", 64, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let conflict_record = fake_folder
        .sync_journal()
        .conflict_record(b"A/a1 (conflicted copy 1234)");
    assert!(conflict_record.is_valid());
    assert_eq!(
        conflict_record.base_file_id,
        remote_file_id(&fake_folder, "A/a1")
    );
    assert_eq!(conflict_record.initial_base_path, b"A/a1");

    // Now with server headers
    let a2_file_id = remote_file_id(&fake_folder, "A/a2");
    {
        let a2_file_id = a2_file_id.clone();
        fake_folder.set_server_override(move |request, state| {
            if request.method() == http::Method::GET {
                let file_name = file_path_from_url(request.uri()).unwrap_or_default();
                let mut reply = get_reply(&state.remote_root, &file_name);
                let headers = reply.headers_mut();
                headers.insert("OC-Conflict", HeaderValue::from_static("1"));
                headers.insert(
                    "OC-ConflictBaseFileId",
                    HeaderValue::from_bytes(&a2_file_id).unwrap(),
                );
                headers.insert("OC-ConflictBaseMtime", HeaderValue::from_static("1234"));
                headers.insert("OC-ConflictBaseEtag", HeaderValue::from_static("etag"));
                headers.insert(
                    "OC-ConflictInitialBasePath",
                    HeaderValue::from_static("A/original"),
                );
                return Some(reply.into());
            }
            None
        });
    }
    fake_folder
        .remote_modifier()
        .insert("A/really-a-conflict", 64, b'W'); // doesn't look like a conflict, but headers say it is
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let conflict_record = fake_folder
        .sync_journal()
        .conflict_record(b"A/really-a-conflict");
    assert!(conflict_record.is_valid());
    assert_eq!(conflict_record.base_file_id, a2_file_id);
    assert_eq!(conflict_record.base_modtime, 1234);
    assert_eq!(conflict_record.base_etag, b"etag");
    assert_eq!(conflict_record.initial_base_path, b"A/original");
}

// Check that conflict records are removed when the file is gone
#[test]
fn test_conflict_record_removal1() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": true }));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Make conflict records
    let mut conflict_record = ConflictRecord {
        path: b"A/a1".to_vec(),
        ..ConflictRecord::default()
    };
    fake_folder
        .sync_journal()
        .set_conflict_record(&conflict_record);
    conflict_record.path = b"A/a2".to_vec();
    fake_folder
        .sync_journal()
        .set_conflict_record(&conflict_record);

    // A nothing-to-sync keeps them alive
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert!(
        fake_folder
            .sync_journal()
            .conflict_record(b"A/a1")
            .is_valid()
    );
    assert!(
        fake_folder
            .sync_journal()
            .conflict_record(b"A/a2")
            .is_valid()
    );

    // When the file is removed, the record is removed too
    fake_folder.local_modifier().remove("A/a2");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert!(
        fake_folder
            .sync_journal()
            .conflict_record(b"A/a1")
            .is_valid()
    );
    assert!(
        !fake_folder
            .sync_journal()
            .conflict_record(b"A/a2")
            .is_valid()
    );
}

// Same test, but with uploadConflictFiles == false
#[test]
fn test_conflict_record_removal2() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": false }));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Create two conflicts
    fake_folder.local_modifier().append_byte("A/a1");
    fake_folder.local_modifier().append_byte("A/a1");
    fake_folder.remote_modifier().append_byte("A/a1");
    fake_folder.local_modifier().append_byte("A/a2");
    fake_folder.local_modifier().append_byte("A/a2");
    fake_folder.remote_modifier().append_byte("A/a2");
    assert!(fake_folder.sync_once());

    let conflicts = find_conflicts(&child(&fake_folder.current_local_state(), "A"));
    let mut a1conflict = String::new();
    let mut a2conflict = String::new();
    for conflict in &conflicts {
        if conflict.contains("a1") {
            a1conflict = conflict.clone();
        }
        if conflict.contains("a2") {
            a2conflict = conflict.clone();
        }
    }

    // A nothing-to-sync keeps them alive
    assert!(fake_folder.sync_once());
    assert!(
        fake_folder
            .sync_journal()
            .conflict_record(a1conflict.as_bytes())
            .is_valid()
    );
    assert!(
        fake_folder
            .sync_journal()
            .conflict_record(a2conflict.as_bytes())
            .is_valid()
    );

    // When the file is removed, the record is removed too
    fake_folder.local_modifier().remove(&a2conflict);
    assert!(fake_folder.sync_once());
    assert!(
        fake_folder
            .sync_journal()
            .conflict_record(a1conflict.as_bytes())
            .is_valid()
    );
    assert!(
        !fake_folder
            .sync_journal()
            .conflict_record(a2conflict.as_bytes())
            .is_valid()
    );
}

fn is_dir(fake_folder: &FakeFolder, path: &str) -> bool {
    Path::new(&format!("{}{}", fake_folder.local_path(), path)).is_dir()
}

fn exists(fake_folder: &FakeFolder, path: &str) -> bool {
    Path::new(&format!("{}{}", fake_folder.local_path(), path)).exists()
}

fn sorted_conflict_records(fake_folder: &FakeFolder) -> Vec<Vec<u8>> {
    let mut records = fake_folder.sync_journal().conflict_record_paths();
    records.sort();
    records
}

#[test]
fn test_local_dir_remote_file_conflict() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": true }));
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    let cleanup = || complete_spy.clear();
    cleanup();

    // 1) a NEW/NEW conflict
    fake_folder.local_modifier().mkdir("Z");
    fake_folder.local_modifier().mkdir("Z/subdir");
    fake_folder.local_modifier().insert("Z/foo", 64, b'W');
    fake_folder.remote_modifier().insert("Z", 63, b'W');

    // 2) local file becomes a dir; remote file changes
    fake_folder.local_modifier().remove("A/a1");
    fake_folder.local_modifier().mkdir("A/a1");
    fake_folder.local_modifier().insert("A/a1/bar", 64, b'W');
    fake_folder.remote_modifier().append_byte("A/a1");

    // 3) local dir gets a new file; remote dir becomes a file
    fake_folder.local_modifier().insert("B/zzz", 64, b'W');
    fake_folder.remote_modifier().remove("B");
    fake_folder.remote_modifier().insert("B", 31, b'W');

    assert!(fake_folder.sync_once());

    let mut conflicts = find_conflicts(&fake_folder.current_local_state());
    conflicts.extend(find_conflicts(&child(
        &fake_folder.current_local_state(),
        "A",
    )));
    assert_eq!(conflicts.len(), 3);
    conflicts.sort();

    let conflict_records = sorted_conflict_records(&fake_folder);
    assert_eq!(conflict_records.len(), 3);

    // 1)
    assert!(item_conflict(&complete_spy, "Z"));
    assert_eq!(
        fake_folder.current_local_state().find("Z").unwrap().size,
        63
    );
    assert!(conflicts[2].contains('Z'));
    assert_eq!(conflicts[2].as_bytes(), conflict_records[2]);
    assert!(is_dir(&fake_folder, &conflicts[2]));
    assert!(exists(&fake_folder, &format!("{}/foo", conflicts[2])));

    // 2)
    assert!(item_conflict(&complete_spy, "A/a1"));
    assert_eq!(
        fake_folder.current_local_state().find("A/a1").unwrap().size,
        5
    );
    assert!(conflicts[0].contains("A/a1"));
    assert_eq!(conflicts[0].as_bytes(), conflict_records[0]);
    assert!(is_dir(&fake_folder, &conflicts[0]));
    assert!(exists(&fake_folder, &format!("{}/bar", conflicts[0])));

    // 3)
    assert!(item_conflict(&complete_spy, "B"));
    assert_eq!(
        fake_folder.current_local_state().find("B").unwrap().size,
        31
    );
    assert!(conflicts[1].contains('B'));
    assert_eq!(conflicts[1].as_bytes(), conflict_records[1]);
    assert!(is_dir(&fake_folder, &conflicts[1]));
    assert!(exists(&fake_folder, &format!("{}/zzz", conflicts[1])));

    // The contents of the conflict directories will only be uploaded after
    // another sync.
    assert!(
        fake_folder.sync_engine().is_another_sync_needed() == AnotherSyncNeeded::ImmediateFollowUp
    );
    cleanup();
    assert!(fake_folder.sync_once());

    assert!(item_successful(
        &complete_spy,
        &conflicts[0],
        Instruction::New
    ));
    assert!(item_successful(
        &complete_spy,
        &format!("{}/bar", conflicts[0]),
        Instruction::New
    ));
    assert!(item_successful(
        &complete_spy,
        &conflicts[1],
        Instruction::New
    ));
    assert!(item_successful(
        &complete_spy,
        &format!("{}/zzz", conflicts[1]),
        Instruction::New
    ));
    assert!(item_successful(
        &complete_spy,
        &conflicts[2],
        Instruction::New
    ));
    assert!(item_successful(
        &complete_spy,
        &format!("{}/foo", conflicts[2]),
        Instruction::New
    ));
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_local_file_remote_dir_conflict() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "uploadConflictFiles": true }));
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    // 1) a NEW/NEW conflict
    fake_folder.remote_modifier().mkdir("Z");
    fake_folder.remote_modifier().mkdir("Z/subdir");
    fake_folder.remote_modifier().insert("Z/foo", 64, b'W');
    fake_folder.local_modifier().insert("Z", 64, b'W');

    // 2) local dir becomes file: remote dir adds file
    fake_folder.local_modifier().remove("A");
    fake_folder.local_modifier().insert("A", 63, b'W');
    fake_folder.remote_modifier().insert("A/bar", 64, b'W');

    // 3) local file changes; remote file becomes dir
    fake_folder.local_modifier().append_byte("B/b1");
    fake_folder.remote_modifier().remove("B/b1");
    fake_folder.remote_modifier().mkdir("B/b1");
    fake_folder.remote_modifier().insert("B/b1/zzz", 64, b'W');

    assert!(fake_folder.sync_once());
    let mut conflicts = find_conflicts(&fake_folder.current_local_state());
    conflicts.extend(find_conflicts(&child(
        &fake_folder.current_local_state(),
        "B",
    )));
    assert_eq!(conflicts.len(), 3);
    conflicts.sort();

    let conflict_records = sorted_conflict_records(&fake_folder);
    assert_eq!(conflict_records.len(), 3);

    // 1)
    assert!(item_conflict(&complete_spy, "Z"));
    assert!(conflicts[2].contains('Z'));
    assert_eq!(conflicts[2].as_bytes(), conflict_records[2]);

    // 2)
    assert!(item_conflict(&complete_spy, "A"));
    assert!(conflicts[0].contains('A'));
    assert_eq!(conflicts[0].as_bytes(), conflict_records[0]);

    // 3)
    assert!(item_conflict(&complete_spy, "B/b1"));
    assert!(conflicts[1].contains("B/b1"));
    assert_eq!(conflicts[1].as_bytes(), conflict_records[1]);

    // Also verifies that conflicts were uploaded
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

/// `QDir(fakeFolder.localPath() + conflict).removeRecursively()`.
fn remove_recursively(fake_folder: &FakeFolder, path: &str) {
    let _ = std::fs::remove_dir_all(format!("{}{}", fake_folder.local_path(), path));
}

#[test]
fn test_type_conflict_with_move() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    // the remote becomes a file, but a file inside the dir has moved away!
    fake_folder.remote_modifier().remove("A");
    fake_folder.remote_modifier().insert("A", 64, b'W');
    fake_folder.local_modifier().rename("A/a1", "a1");

    // same, but with a new file inside the dir locally
    fake_folder.remote_modifier().remove("B");
    fake_folder.remote_modifier().insert("B", 64, b'W');
    fake_folder.local_modifier().rename("B/b1", "b1");
    fake_folder.local_modifier().insert("B/new", 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(item_successful(&complete_spy, "A", Instruction::TypeChange));
    assert!(item_conflict(&complete_spy, "B"));

    let mut conflicts = find_conflicts(&fake_folder.current_local_state());
    conflicts.sort();
    assert!(conflicts.len() == 2);
    assert!(conflicts[0].contains("A (conflicted copy"));
    assert!(conflicts[1].contains("B (conflicted copy"));
    for conflict in &conflicts {
        remove_recursively(&fake_folder, conflict);
    }
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Currently a1 and b1 don't get moved, but redownloaded
}

#[test]
fn test_type_change() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    // dir becomes file
    fake_folder.remote_modifier().remove("A");
    fake_folder.remote_modifier().insert("A", 64, b'W');
    fake_folder.local_modifier().remove("B");
    fake_folder.local_modifier().insert("B", 64, b'W');

    // file becomes dir
    fake_folder.remote_modifier().remove("C/c1");
    fake_folder.remote_modifier().mkdir("C/c1");
    fake_folder.remote_modifier().insert("C/c1/foo", 64, b'W');
    fake_folder.local_modifier().remove("C/c2");
    fake_folder.local_modifier().mkdir("C/c2");
    fake_folder.local_modifier().insert("C/c2/bar", 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(item_successful(&complete_spy, "A", Instruction::TypeChange));
    assert!(item_successful(&complete_spy, "B", Instruction::TypeChange));
    assert!(item_successful(
        &complete_spy,
        "C/c1",
        Instruction::TypeChange
    ));
    assert!(item_successful(
        &complete_spy,
        "C/c2",
        Instruction::TypeChange
    ));

    // A becomes a conflict because we don't delete folders with files
    // inside of them!
    let conflicts = find_conflicts(&fake_folder.current_local_state());
    assert!(conflicts.len() == 1);
    assert!(conflicts[0].contains("A (conflicted copy"));
    for conflict in &conflicts {
        remove_recursively(&fake_folder, conflict);
    }

    assert!(
        fake_folder.sync_engine().is_another_sync_needed() == AnotherSyncNeeded::ImmediateFollowUp
    );
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_etag_change_file_not_changed_generates_no_conflicts() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    // (upstream passes 'W' as the size argument: insert(path, size = 'W'))
    fake_folder
        .remote_modifier()
        .insert("A/fake_conflict", i64::from(b'W'), b'W');
    assert!(fake_folder.sync_once());
    assert!(!item_conflict(&complete_spy, "A/fake_conflict"));

    complete_spy.clear();

    fake_folder
        .remote_modifier()
        .set_contents("A/fake_conflict", b'W');
    fake_folder
        .local_modifier()
        .set_contents("A/fake_conflict", b'W');

    assert!(fake_folder.sync_once());
    assert!(!item_conflict(&complete_spy, "A/fake_conflict"));
}

#[test]
fn test_etag_change_file_changed_generates_conflicts() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    // (upstream passes 'W' as the size argument: insert(path, size = 'W'))
    fake_folder
        .remote_modifier()
        .insert("A/real_conflict", i64::from(b'W'), b'W');
    assert!(fake_folder.sync_once());
    assert!(!item_conflict(&complete_spy, "A/real_conflict"));

    complete_spy.clear();

    fake_folder
        .remote_modifier()
        .set_contents("A/real_conflict", b'W');
    fake_folder
        .local_modifier()
        .set_contents("A/real_conflict", b'L');

    assert!(fake_folder.sync_once());
    assert!(item_conflict(&complete_spy, "A/real_conflict"));
}

// Test what happens if we remove entries both on the server, and locally
#[test]
fn test_remove_remove() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.remote_modifier().remove("A");
    fake_folder.local_modifier().remove("A");
    fake_folder.remote_modifier().remove("B/b1");
    fake_folder.local_modifier().remove("B/b1");

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    let expected_state = fake_folder.current_local_state();

    assert!(fake_folder.sync_once());

    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);

    assert!(db_record(&fake_folder, "B/b2").is_valid());

    assert!(!db_record(&fake_folder, "B/b1").is_valid());
    assert!(!db_record(&fake_folder, "A/a1").is_valid());
    assert!(!db_record(&fake_folder, "A").is_valid());
}
