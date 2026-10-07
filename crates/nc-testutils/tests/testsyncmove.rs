// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testsyncmove.cpp (nextcloud/desktop v34.0.5).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use nc_journal::journal::SelectiveSyncListType;
use nc_journal::remote_permissions::RemotePermissions;
use nc_sync::discovery::LocalDiscoveryStyle;
use nc_sync::propagator::is_path_inside_deleted_dir;
use nc_sync::{Direction, ErrorCategory, Instruction, Status, SyncFileItemPtr, SyncFileItemVector};
use nc_testutils::server::error_reply;
use nc_testutils::{
    FakeFolder, FakeReply, FileInfo, FileModifier, ItemCompletedSpy, PathComponents, print_db_data,
};

#[derive(Default)]
struct Counts {
    n_get: AtomicI32,
    n_put: AtomicI32,
    n_move: AtomicI32,
    n_delete: AtomicI32,
    n_propfind: AtomicI32,
    n_mkcol: AtomicI32,
}

/// `OperationCounter`: counts the requests per verb (shared with the server
/// override).
#[derive(Clone, Default)]
struct OperationCounter(Arc<Counts>);

impl OperationCounter {
    fn reset(&self) {
        for c in [
            &self.0.n_get,
            &self.0.n_put,
            &self.0.n_move,
            &self.0.n_delete,
            &self.0.n_propfind,
            &self.0.n_mkcol,
        ] {
            c.store(0, Ordering::SeqCst);
        }
    }

    fn n_get(&self) -> i32 {
        self.0.n_get.load(Ordering::SeqCst)
    }
    fn n_put(&self) -> i32 {
        self.0.n_put.load(Ordering::SeqCst)
    }
    fn n_move(&self) -> i32 {
        self.0.n_move.load(Ordering::SeqCst)
    }
    fn n_delete(&self) -> i32 {
        self.0.n_delete.load(Ordering::SeqCst)
    }
    fn n_mkcol(&self) -> i32 {
        self.0.n_mkcol.load(Ordering::SeqCst)
    }

    /// `functor()`: installs the counting override on the fake server.
    fn install(&self, ff: &FakeFolder) {
        let counts = self.0.clone();
        ff.set_server_override(move |req, _| {
            let c = match req.method().as_str() {
                "GET" => &counts.n_get,
                "PUT" => &counts.n_put,
                "DELETE" => &counts.n_delete,
                "MOVE" => &counts.n_move,
                "PROPFIND" => &counts.n_propfind,
                "MKCOL" => &counts.n_mkcol,
                _ => return None,
            };
            c.fetch_add(1, Ordering::SeqCst);
            None
        });
    }
}

fn item_successful(spy: &ItemCompletedSpy, path: &str, instr: Instruction) -> bool {
    let item = spy.find_item(path);
    item.status == Status::Success && item.instruction == instr
}

fn item_conflict(spy: &ItemCompletedSpy, path: &str) -> bool {
    let item = spy.find_item(path);
    item.status == Status::Conflict && item.instruction == Instruction::Conflict
}

fn item_successful_move(spy: &ItemCompletedSpy, path: &str) -> bool {
    item_successful(spy, path, Instruction::Rename)
}

fn find_conflicts(dir: &FileInfo) -> Vec<String> {
    dir.children
        .values()
        .filter(|i| i.name.contains("(conflicted copy"))
        .map(|i| i.path())
        .collect()
}

fn expect_and_wipe_conflict(local: &mut dyn FileModifier, state: &FileInfo, path: &str) -> bool {
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

/// `expectAndWipeConflict(local, fakeFolder.currentLocalState(), path)`.
fn wipe_conflict(ff: &mut FakeFolder, path: &str) -> bool {
    let state = ff.current_local_state();
    expect_and_wipe_conflict(ff.local_modifier(), &state, path)
}

fn set_all_perm(fi: &mut FileInfo, perm: RemotePermissions) {
    fi.permissions = perm;
    for sub_fi in fi.children.values_mut() {
        set_all_perm(sub_fi, perm);
    }
}

fn assert_local_eq_remote(ff: &FakeFolder) {
    assert_eq!(ff.current_local_state(), ff.current_remote_state());
}

fn assert_db_eq_remote(ff: &FakeFolder) {
    assert_eq!(
        print_db_data(&ff.db_state()),
        print_db_data(&ff.current_remote_state())
    );
}

fn remote_char(ff: &FakeFolder, path: &str) -> u8 {
    ff.remote_modifier().find(path).unwrap().content_char
}

/// `FileInfo{ QString(), { FileInfo{ "folder", { FileInfo{ "folderA", { { "file.txt", 400 } } }, "folderB" } } } }`
fn folder_a_b_tree() -> FileInfo {
    FileInfo::dir(
        "",
        [FileInfo::dir(
            "folder",
            [
                FileInfo::dir("folderA", [FileInfo::file("file.txt", 400)]),
                FileInfo::named("folderB"),
            ],
        )],
    )
}

/// `FileInfo{QString{}, {FileInfo{"D", {{"file.txt", 64}}}}}`
fn d_file_tree() -> FileInfo {
    FileInfo::dir("", [FileInfo::dir("D", [FileInfo::file("file.txt", 64)])])
}

fn seed_stale_lock(folder: &FakeFolder, path: &str) {
    let mut record = folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .expect("getFileRecord")
        .unwrap_or_default();
    record.lockstate.locked = true;
    record.lockstate.lock_token = "stale-token".to_owned();
    assert!(folder.sync_journal().set_file_record(&record).is_ok());
}

fn db_record(ff: &FakeFolder, path: &str) -> nc_journal::journal::SyncJournalFileRecord {
    ff.sync_journal()
        .get_file_record(path.as_bytes())
        .expect("getFileRecord")
        .unwrap_or_default()
}

fn count_db_records(ff: &FakeFolder) -> i32 {
    let mut items_counter = 0;
    let db_result = ff
        .sync_journal()
        .get_files_below_path(b"", |_| items_counter += 1);
    assert!(db_result.is_ok());
    items_counter
}

#[test]
fn test_move_custom_remote_root() {
    let sub_folder = FileInfo::dir("AS", [FileInfo::file("f1", 4)]);
    let folder = FileInfo::dir("A", [sub_folder]);
    let file_info = FileInfo::dir("", [folder.clone()]);

    let mut fake_folder = FakeFolder::with_options(file_info, Some(&folder), "/A", true);

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // Move file and then move it back again
    {
        counter.reset();
        fake_folder.local_modifier().rename("AS/f1", "f1");

        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());

        assert_eq!(counter.n_get(), 0);
        assert_eq!(counter.n_put(), 0);
        assert_eq!(counter.n_move(), 1);
        assert_eq!(counter.n_delete(), 0);

        assert!(item_successful(&complete_spy, "f1", Instruction::Rename));
        assert!(fake_folder.current_remote_state().find("A/f1").is_some());
        assert!(fake_folder.current_remote_state().find("A/AS/f1").is_none());
    }
}

#[test]
fn test_remote_change_in_moved_folder() {
    // issue #5192
    let mut fake_folder = FakeFolder::new(folder_a_b_tree());

    assert_local_eq_remote(&fake_folder);

    // Edit a file in a moved directory.
    fake_folder
        .remote_modifier()
        .set_contents("folder/folderA/file.txt", b'a');
    fake_folder
        .remote_modifier()
        .rename("folder/folderA", "folder/folderB/folderA");
    fake_folder.sync_once();
    assert_local_eq_remote(&fake_folder);
    let old_state = fake_folder.current_local_state();
    assert!(old_state.find("folder/folderB/folderA/file.txt").is_some());
    assert!(old_state.find("folder/folderA/file.txt").is_none());

    // This sync should not remove the file
    fake_folder.sync_once();
    assert_local_eq_remote(&fake_folder);
    assert_eq!(fake_folder.current_local_state(), old_state);
}

#[test]
fn test_selective_sync_moved_folder() {
    // issue #5224
    let mut fake_folder = FakeFolder::new(FileInfo::dir(
        "",
        [FileInfo::dir(
            "parentFolder",
            [
                FileInfo::dir("subFolderA", [FileInfo::file("fileA.txt", 400)]),
                FileInfo::dir("subFolderB", [FileInfo::file("fileB.txt", 400)]),
            ],
        )],
    ));

    assert_local_eq_remote(&fake_folder);
    let mut expected_server_state = fake_folder.current_remote_state();

    // Remove subFolderA with selectiveSync:
    fake_folder.sync_journal().set_selective_sync_list(
        SelectiveSyncListType::BlackList,
        &["parentFolder/subFolderA/".to_owned()],
    );
    fake_folder
        .sync_journal()
        .schedule_path_for_remote_discovery(b"parentFolder/subFolderA/");

    fake_folder.sync_once();

    {
        // Nothing changed on the server
        assert_eq!(fake_folder.current_remote_state(), expected_server_state);
        // The local state should not have subFolderA
        let mut remote_state = fake_folder.current_remote_state();
        remote_state.remove("parentFolder/subFolderA");
        assert_eq!(fake_folder.current_local_state(), remote_state);
    }

    // Rename parentFolder on the server
    fake_folder
        .remote_modifier()
        .rename("parentFolder", "parentFolderRenamed");
    expected_server_state = fake_folder.current_remote_state();
    fake_folder.sync_once();

    {
        assert_eq!(fake_folder.current_remote_state(), expected_server_state);
        let mut remote_state = fake_folder.current_remote_state();
        // The subFolderA should still be there on the server.
        assert!(
            remote_state
                .find("parentFolderRenamed/subFolderA/fileA.txt")
                .is_some()
        );
        // But not on the client because of the selective sync
        remote_state.remove("parentFolderRenamed/subFolderA");
        assert_eq!(fake_folder.current_local_state(), remote_state);
    }

    // Rename it again, locally this time.
    fake_folder
        .local_modifier()
        .rename("parentFolderRenamed", "parentThirdName");
    fake_folder.sync_once();

    {
        let mut remote_state = fake_folder.current_remote_state();
        // The subFolderA should still be there on the server.
        assert!(
            remote_state
                .find("parentThirdName/subFolderA/fileA.txt")
                .is_some()
        );
        // But not on the client because of the selective sync
        remote_state.remove("parentThirdName/subFolderA");
        assert_eq!(fake_folder.current_local_state(), remote_state);

        expected_server_state = fake_folder.current_remote_state();
        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        fake_folder.sync_once(); // This sync should do nothing
        assert_eq!(complete_spy.len(), 0);

        assert_eq!(fake_folder.current_remote_state(), expected_server_state);
        assert_eq!(fake_folder.current_local_state(), remote_state);
    }
}

#[test]
fn test_local_move_detection() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // For directly editing the remote checksum
    // (`remoteInfo` is `fakeFolder.currentRemoteState()` here.)

    // Simple move causing a remote rename
    fake_folder.local_modifier().rename("A/a1", "A/a1m");
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_put(), 0);

    // Move-and-change, causing a upload and delete
    fake_folder.local_modifier().rename("A/a2", "A/a2m");
    fake_folder.local_modifier().append_byte("A/a2m");
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_put(), 1);
    assert_eq!(counter.n_delete(), 1);

    // Move-and-change, mtime+content only
    fake_folder.local_modifier().rename("B/b1", "B/b1m");
    fake_folder.local_modifier().set_contents("B/b1m", b'C');
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_put(), 2);
    assert_eq!(counter.n_delete(), 2);

    // Move-and-change, size+content only
    let mut mtime = fake_folder
        .remote_modifier()
        .find("B/b2")
        .unwrap()
        .last_modified;
    fake_folder.local_modifier().rename("B/b2", "B/b2m");
    fake_folder.local_modifier().append_byte("B/b2m");
    fake_folder.local_modifier().set_mod_time("B/b2m", mtime);
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_put(), 3);
    assert_eq!(counter.n_delete(), 3);

    // Move-and-change, content only -- c1 has no checksum, so we fail to detect this!
    // NOTE: This is an expected failure.
    mtime = fake_folder
        .remote_modifier()
        .find("C/c1")
        .unwrap()
        .last_modified;
    fake_folder.local_modifier().rename("C/c1", "C/c1m");
    fake_folder.local_modifier().set_contents("C/c1m", b'C');
    fake_folder.local_modifier().set_mod_time("C/c1m", mtime);
    assert!(fake_folder.sync_once());
    assert_eq!(counter.n_put(), 3);
    assert_eq!(counter.n_delete(), 3);
    assert!(fake_folder.current_local_state() != fake_folder.current_remote_state());

    // cleanup, and upload a file that will have a checksum in the db
    fake_folder.local_modifier().remove("C/c1m");
    fake_folder.local_modifier().insert("C/c3", 64, b'W');
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_put(), 4);
    assert_eq!(counter.n_delete(), 4);

    // Move-and-change, content only, this time while having a checksum
    mtime = fake_folder
        .remote_modifier()
        .find("C/c3")
        .unwrap()
        .last_modified;
    fake_folder.local_modifier().rename("C/c3", "C/c3m");
    fake_folder.local_modifier().set_contents("C/c3m", b'C');
    fake_folder.local_modifier().set_mod_time("C/c3m", mtime);
    assert!(fake_folder.sync_once());
    assert_eq!(counter.n_put(), 5);
    assert_eq!(counter.n_delete(), 5);
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
}

#[test]
fn test_local_external_storage_rename_detection() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().mkdir("external-storage");
    {
        let mut remote = fake_folder.remote_modifier();
        let external_storage = remote.find_mut("external-storage").unwrap();
        external_storage.extra_dav_properties =
            "<nc:is-mount-root>true</nc:is-mount-root>".to_owned();
        set_all_perm(
            external_storage,
            RemotePermissions::from_server_string("WDNVCKRMG"),
        );
    }
    assert!(fake_folder.sync_once());

    let operation_counter = OperationCounter::default();
    operation_counter.install(&fake_folder);

    fake_folder
        .local_modifier()
        .insert("external-storage/file", 100, b'W');
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(operation_counter.n_put(), 1);

    let first_file_id = fake_folder
        .remote_modifier()
        .find("external-storage/file")
        .unwrap()
        .file_id
        .clone();

    fake_folder
        .local_modifier()
        .rename("external-storage/file", "external-storage/file2");
    assert!(fake_folder.sync_once());
    assert_eq!(operation_counter.n_move(), 1);

    fake_folder
        .local_modifier()
        .rename("external-storage/file2", "external-storage/file3");
    assert!(fake_folder.sync_once());
    assert_eq!(operation_counter.n_move(), 2);

    let renamed_file_id = fake_folder
        .remote_modifier()
        .find("external-storage/file3")
        .unwrap()
        .file_id
        .clone();

    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);

    assert!(
        fake_folder
            .remote_modifier()
            .find("external-storage/file")
            .is_none()
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find("external-storage/file2")
            .is_none()
    );
    assert!(
        fake_folder
            .remote_modifier()
            .find("external-storage/file3")
            .is_some()
    );
    assert_eq!(first_file_id, renamed_file_id);
}

// If the same folder is shared in two different ways with the same
// user, the target user will see duplicate file ids. We need to make
// sure the move detection and sync still do the right thing in that
// case.
fn duplicate_file_id(prefix: &str) {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("A/W");
        remote.insert("A/W/w1", 64, b'W');
        remote.mkdir("A/Q");

        // Duplicate every entry in A under O/A
        remote.mkdir(prefix);
        let a = remote.children["A"].clone();
        remote.children.get_mut(prefix).unwrap().add_child(a);
    }

    // This already checks that the rename detection doesn't get
    // horribly confused if we add new files that have the same
    // fileid as existing ones
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // Try a remote file move
    fake_folder.remote_modifier().rename("A/a1", "A/W/a1m");
    fake_folder
        .remote_modifier()
        .rename(&format!("{prefix}/A/a1"), &format!("{prefix}/A/W/a1m"));
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 0);

    // And a remote directory move
    fake_folder.remote_modifier().rename("A/W", "A/Q/W");
    fake_folder
        .remote_modifier()
        .rename(&format!("{prefix}/A/W"), &format!("{prefix}/A/Q/W"));
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 0);

    // Partial file removal (in practice, A/a2 may be moved to O/a2, but we don't care)
    fake_folder
        .remote_modifier()
        .rename(&format!("{prefix}/A/a2"), &format!("{prefix}/a2"));
    fake_folder.remote_modifier().remove("A/a2");
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 0);

    // Local change plus remote move at the same time
    fake_folder
        .local_modifier()
        .append_byte(&format!("{prefix}/a2"));
    fake_folder
        .remote_modifier()
        .rename(&format!("{prefix}/a2"), &format!("{prefix}/a3"));
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 1);
    counter.reset();

    // remove locally, and remote move at the same time
    fake_folder.local_modifier().remove("A/Q/W/a1m");
    fake_folder
        .remote_modifier()
        .rename("A/Q/W/a1m", "A/Q/W/a1p");
    fake_folder.remote_modifier().rename(
        &format!("{prefix}/A/Q/W/a1m"),
        &format!("{prefix}/A/Q/W/a1p"),
    );
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 1);
    counter.reset();
}

#[test]
fn test_duplicate_file_id() {
    // There have been bugs related to how the original
    // folder and the folder with the duplicate tree are
    // ordered. Test both cases here.
    // "first ordering"
    duplicate_file_id("O"); // "O" > "A"
    // "second ordering"
    duplicate_file_id("0"); // "0" < "A"
}

#[test]
fn test_move_propagation() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // Move
    {
        counter.reset();
        fake_folder.local_modifier().rename("A/a1", "A/a1m");
        fake_folder.remote_modifier().rename("B/b1", "B/b1m");
        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());
        assert_local_eq_remote(&fake_folder);
        assert_eq!(counter.n_get(), 0);
        assert_eq!(counter.n_put(), 0);
        assert_eq!(counter.n_move(), 1);
        assert_eq!(counter.n_delete(), 0);
        assert!(item_successful_move(&complete_spy, "A/a1m"));
        assert!(item_successful_move(&complete_spy, "B/b1m"));
        assert_eq!(complete_spy.find_item("A/a1m").file, "A/a1");
        assert_eq!(complete_spy.find_item("A/a1m").rename_target, "A/a1m");
        assert_eq!(complete_spy.find_item("B/b1m").file, "B/b1");
        assert_eq!(complete_spy.find_item("B/b1m").rename_target, "B/b1m");
    }

    // Touch+Move on same side
    counter.reset();
    fake_folder.local_modifier().rename("A/a2", "A/a2m");
    fake_folder.local_modifier().set_contents("A/a2m", b'A');
    fake_folder.remote_modifier().rename("B/b2", "B/b2m");
    fake_folder.remote_modifier().set_contents("B/b2m", b'A');
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 1);
    assert_eq!(counter.n_put(), 1);
    assert_eq!(counter.n_move(), 0);
    assert_eq!(counter.n_delete(), 1);
    assert_eq!(remote_char(&fake_folder, "A/a2m"), b'A');
    assert_eq!(remote_char(&fake_folder, "B/b2m"), b'A');

    // Touch+Move on opposite sides
    counter.reset();
    fake_folder.local_modifier().rename("A/a1m", "A/a1m2");
    fake_folder.remote_modifier().set_contents("A/a1m", b'B');
    fake_folder.remote_modifier().rename("B/b1m", "B/b1m2");
    fake_folder.local_modifier().set_contents("B/b1m", b'B');
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 2);
    assert_eq!(counter.n_put(), 2);
    assert_eq!(counter.n_move(), 0);
    assert_eq!(counter.n_delete(), 0);
    // All these files existing afterwards is debatable. Should we propagate
    // the rename in one direction and grab the new contents in the other?
    // Currently there's no propagation job that would do that, and this does
    // at least not lose data.
    assert_eq!(remote_char(&fake_folder, "A/a1m"), b'B');
    assert_eq!(remote_char(&fake_folder, "B/b1m"), b'B');
    assert_eq!(remote_char(&fake_folder, "A/a1m2"), b'W');
    assert_eq!(remote_char(&fake_folder, "B/b1m2"), b'W');

    // Touch+create on one side, move on the other
    {
        counter.reset();
        fake_folder.local_modifier().append_byte("A/a1m");
        fake_folder.local_modifier().insert("A/a1mt", 64, b'W');
        fake_folder.remote_modifier().rename("A/a1m", "A/a1mt");
        fake_folder.remote_modifier().append_byte("B/b1m");
        fake_folder.remote_modifier().insert("B/b1mt", 64, b'W');
        fake_folder.local_modifier().rename("B/b1m", "B/b1mt");
        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());
        assert!(wipe_conflict(&mut fake_folder, "A/a1mt"));
        assert!(wipe_conflict(&mut fake_folder, "B/b1mt"));
        assert_local_eq_remote(&fake_folder);
        assert_db_eq_remote(&fake_folder);
        assert_eq!(counter.n_get(), 3);
        assert_eq!(counter.n_put(), 1);
        assert_eq!(counter.n_move(), 0);
        assert_eq!(counter.n_delete(), 0);
        assert!(item_successful(&complete_spy, "A/a1m", Instruction::New));
        assert!(item_successful(&complete_spy, "B/b1m", Instruction::New));
        assert!(item_conflict(&complete_spy, "A/a1mt"));
        assert!(item_conflict(&complete_spy, "B/b1mt"));
    }

    // Create new on one side, move to new on the other
    {
        counter.reset();
        fake_folder.local_modifier().insert("A/a1N", 13, b'W');
        fake_folder.remote_modifier().rename("A/a1mt", "A/a1N");
        fake_folder.remote_modifier().insert("B/b1N", 13, b'W');
        fake_folder.local_modifier().rename("B/b1mt", "B/b1N");
        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());
        assert!(wipe_conflict(&mut fake_folder, "A/a1N"));
        assert!(wipe_conflict(&mut fake_folder, "B/b1N"));
        assert_local_eq_remote(&fake_folder);
        assert_db_eq_remote(&fake_folder);
        assert_eq!(counter.n_get(), 2);
        assert_eq!(counter.n_put(), 0);
        assert_eq!(counter.n_move(), 0);
        assert_eq!(counter.n_delete(), 1);
        assert!(item_successful(
            &complete_spy,
            "A/a1mt",
            Instruction::Remove
        ));
        assert!(item_successful(
            &complete_spy,
            "B/b1mt",
            Instruction::Remove
        ));
        assert!(item_conflict(&complete_spy, "A/a1N"));
        assert!(item_conflict(&complete_spy, "B/b1N"));
    }

    // Local move, remote move
    counter.reset();
    fake_folder.local_modifier().rename("C/c1", "C/c1mL");
    fake_folder.remote_modifier().rename("C/c1", "C/c1mR");
    assert!(fake_folder.sync_once());
    // end up with both files
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 1);
    assert_eq!(counter.n_put(), 1);
    assert_eq!(counter.n_move(), 0);
    assert_eq!(counter.n_delete(), 0);

    // Rename/rename conflict on a folder
    counter.reset();
    fake_folder.remote_modifier().rename("C", "CMR");
    fake_folder.local_modifier().rename("C", "CML");
    assert!(fake_folder.sync_once());
    // End up with both folders
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 3); // 3 files in C
    assert_eq!(counter.n_put(), 3);
    assert_eq!(counter.n_move(), 0);
    assert_eq!(counter.n_delete(), 0);

    // Folder move
    {
        counter.reset();
        fake_folder.local_modifier().rename("A", "AM");
        fake_folder.remote_modifier().rename("B", "BM");
        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());
        assert_local_eq_remote(&fake_folder);
        assert_db_eq_remote(&fake_folder);
        assert_eq!(counter.n_get(), 0);
        assert_eq!(counter.n_put(), 0);
        assert_eq!(counter.n_move(), 1);
        assert_eq!(counter.n_delete(), 0);
        assert!(item_successful_move(&complete_spy, "AM"));
        assert!(item_successful_move(&complete_spy, "BM"));
        assert_eq!(complete_spy.find_item("AM").file, "A");
        assert_eq!(complete_spy.find_item("AM").rename_target, "AM");
        assert_eq!(complete_spy.find_item("BM").file, "B");
        assert_eq!(complete_spy.find_item("BM").rename_target, "BM");
    }

    // Folder move with contents touched on the same side
    {
        counter.reset();
        fake_folder.local_modifier().set_contents("AM/a2m", b'C');
        // We must change the modtime for it is likely that it did not change between sync.
        // (Previous version of the client (<=2.5) would not need this because it was always doing
        // checksum comparison for all renames. But newer version no longer does it if the file is
        // renamed because the parent folder is renamed)
        fake_folder.local_modifier().set_mod_time(
            "AM/a2m",
            SystemTime::now() + Duration::from_secs(3 * 24 * 3600),
        );
        fake_folder.local_modifier().rename("AM", "A2");
        fake_folder.remote_modifier().set_contents("BM/b2m", b'C');
        fake_folder.remote_modifier().rename("BM", "B2");
        let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());
        assert_local_eq_remote(&fake_folder);
        assert_db_eq_remote(&fake_folder);
        assert_eq!(counter.n_get(), 1);
        assert_eq!(counter.n_put(), 1);
        assert_eq!(counter.n_move(), 1);
        assert_eq!(counter.n_delete(), 0);
        assert_eq!(remote_char(&fake_folder, "A2/a2m"), b'C');
        assert_eq!(remote_char(&fake_folder, "B2/b2m"), b'C');
        assert!(item_successful_move(&complete_spy, "A2"));
        assert!(item_successful_move(&complete_spy, "B2"));
    }

    // Folder rename with contents touched on the other tree
    counter.reset();
    fake_folder.remote_modifier().set_contents("A2/a2m", b'D');
    // setContents alone may not produce updated mtime if the test is fast
    // and since we don't use checksums here, that matters.
    fake_folder.remote_modifier().append_byte("A2/a2m");
    fake_folder.local_modifier().rename("A2", "A3");
    fake_folder.local_modifier().set_contents("B2/b2m", b'D');
    fake_folder.local_modifier().append_byte("B2/b2m");
    fake_folder.remote_modifier().rename("B2", "B3");
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 1);
    assert_eq!(counter.n_put(), 1);
    assert_eq!(counter.n_move(), 1);
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(remote_char(&fake_folder, "A3/a2m"), b'D');
    assert_eq!(remote_char(&fake_folder, "B3/b2m"), b'D');

    // Folder rename with contents touched on both ends
    counter.reset();
    fake_folder.remote_modifier().set_contents("A3/a2m", b'R');
    fake_folder.remote_modifier().append_byte("A3/a2m");
    fake_folder.local_modifier().set_contents("A3/a2m", b'L');
    fake_folder.local_modifier().append_byte("A3/a2m");
    fake_folder.local_modifier().append_byte("A3/a2m");
    fake_folder.local_modifier().rename("A3", "A4");
    fake_folder.remote_modifier().set_contents("B3/b2m", b'R');
    fake_folder.remote_modifier().append_byte("B3/b2m");
    fake_folder.local_modifier().set_contents("B3/b2m", b'L');
    fake_folder.local_modifier().append_byte("B3/b2m");
    fake_folder.local_modifier().append_byte("B3/b2m");
    fake_folder.remote_modifier().rename("B3", "B4");
    assert!(fake_folder.sync_once());
    let current_local = fake_folder.current_local_state();
    let mut conflicts = find_conflicts(&current_local.children["A4"]);
    assert_eq!(conflicts.len(), 1);
    for c in &conflicts {
        assert_eq!(current_local.find(c.as_str()).unwrap().content_char, b'L');
        fake_folder.local_modifier().remove(c);
    }
    conflicts = find_conflicts(&current_local.children["B4"]);
    assert_eq!(conflicts.len(), 1);
    for c in &conflicts {
        assert_eq!(current_local.find(c.as_str()).unwrap().content_char, b'L');
        fake_folder.local_modifier().remove(c);
    }
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 2);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 1);
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(remote_char(&fake_folder, "A4/a2m"), b'R');
    assert_eq!(remote_char(&fake_folder, "B4/b2m"), b'R');

    // Rename a folder and rename the contents at the same time
    counter.reset();
    fake_folder.local_modifier().rename("A4/a2m", "A4/a2m2");
    fake_folder.local_modifier().rename("A4", "A5");
    fake_folder.remote_modifier().rename("B4/b2m", "B4/b2m2");
    fake_folder.remote_modifier().rename("B4", "B5");
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 2);
    assert_eq!(counter.n_delete(), 0);
}

// These renames can be troublesome on windows
#[test]
fn test_rename_case_only() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    fake_folder.local_modifier().rename("A/a1", "A/A1");
    fake_folder.remote_modifier().rename("A/a2", "A/A2");

    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    assert_db_eq_remote(&fake_folder);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 1);
    assert_eq!(counter.n_delete(), 0);
}

// Check interaction of moves with file type changes
#[test]
fn test_move_and_type_change() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    // Touch on one side, rename and mkdir on the other
    {
        fake_folder.local_modifier().append_byte("A/a1");
        fake_folder.remote_modifier().rename("A/a1", "A/a1mq");
        fake_folder.remote_modifier().mkdir("A/a1");
        fake_folder.remote_modifier().append_byte("B/b1");
        fake_folder.local_modifier().rename("B/b1", "B/b1mq");
        fake_folder.local_modifier().mkdir("B/b1");
        let _complete_spy = ItemCompletedSpy::new(&mut fake_folder);
        assert!(fake_folder.sync_once());
        // BUG: This doesn't behave right
        //QCOMPARE(fakeFolder.currentLocalState(), fakeFolder.currentRemoteState());
    }
}

// Test for https://github.com/owncloud/client/issues/6694
#[test]
fn test_invert_folder_hierarchy() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    {
        let mut r = fake_folder.remote_modifier();
        r.mkdir("A/Empty");
        r.mkdir("A/Empty/Foo");
        r.mkdir("C/AllEmpty");
        r.mkdir("C/AllEmpty/Bar");
        r.insert("A/Empty/f1", 64, b'W');
        r.insert("A/Empty/Foo/f2", 64, b'W');
        r.mkdir("C/AllEmpty/f3");
        r.mkdir("C/AllEmpty/Bar/f4");
    }
    assert!(fake_folder.sync_once());

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // "Empty" is after "A", alphabetically
    fake_folder.local_modifier().rename("A/Empty", "Empty");
    fake_folder.local_modifier().rename("A", "Empty/A");

    // "AllEmpty" is before "C", alphabetically
    fake_folder
        .local_modifier()
        .rename("C/AllEmpty", "AllEmpty");
    fake_folder.local_modifier().rename("C", "AllEmpty/C");

    let mut expected_state = fake_folder.current_local_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);

    // Now, the revert, but "crossed"
    fake_folder.local_modifier().rename("Empty/A", "A");
    fake_folder.local_modifier().rename("AllEmpty/C", "C");
    fake_folder.local_modifier().rename("Empty", "C/Empty");
    fake_folder
        .local_modifier()
        .rename("AllEmpty", "A/AllEmpty");
    expected_state = fake_folder.current_local_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);

    // Reverse on remote
    {
        let mut r = fake_folder.remote_modifier();
        r.rename("A/AllEmpty", "AllEmpty");
        r.rename("C/Empty", "Empty");
        r.rename("C", "AllEmpty/C");
        r.rename("A", "Empty/A");
    }
    expected_state = fake_folder.current_remote_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), expected_state);
    assert_eq!(fake_folder.current_remote_state(), expected_state);
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
}

/// `local ? fakeFolder.localModifier() : fakeFolder.remoteModifier()`.
fn with_modifier(ff: &mut FakeFolder, local: bool, f: impl FnOnce(&mut dyn FileModifier)) {
    if local {
        f(ff.local_modifier());
    } else {
        f(&mut *ff.remote_modifier());
    }
}

fn deep_hierarchy(local: bool) {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    with_modifier(&mut fake_folder, local, |modifier| {
        modifier.mkdir("FolA");
        modifier.mkdir("FolA/FolB");
        modifier.mkdir("FolA/FolB/FolC");
        modifier.mkdir("FolA/FolB/FolC/FolD");
        modifier.mkdir("FolA/FolB/FolC/FolD/FolE");
        modifier.insert("FolA/FileA.txt", 64, b'W');
        modifier.insert("FolA/FolB/FileB.txt", 64, b'W');
        modifier.insert("FolA/FolB/FolC/FileC.txt", 64, b'W');
        modifier.insert("FolA/FolB/FolC/FolD/FileD.txt", 64, b'W');
        modifier.insert("FolA/FolB/FolC/FolD/FolE/FileE.txt", 64, b'W');
    });
    assert!(fake_folder.sync_once());

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    with_modifier(&mut fake_folder, local, |modifier| {
        modifier.insert("FolA/FileA2.txt", 64, b'W');
        modifier.insert("FolA/FolB/FileB2.txt", 64, b'W');
        modifier.insert("FolA/FolB/FolC/FileC2.txt", 64, b'W');
        modifier.insert("FolA/FolB/FolC/FolD/FileD2.txt", 64, b'W');
        modifier.insert("FolA/FolB/FolC/FolD/FolE/FileE2.txt", 64, b'W');
        modifier.rename("FolA", "FolA_Renamed");
        modifier.rename("FolA_Renamed/FolB", "FolB_Renamed");
        modifier.rename("FolB_Renamed/FolC", "FolA");
        modifier.rename("FolA/FolD", "FolA/FolD_Renamed");
        modifier.mkdir("FolB_Renamed/New");
        modifier.rename("FolA/FolD_Renamed/FolE", "FolB_Renamed/New/FolE");
    });
    let expected = if local {
        fake_folder.current_local_state()
    } else {
        fake_folder.current_remote_state()
    };
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), expected);
    assert_eq!(fake_folder.current_remote_state(), expected);
    assert_eq!(counter.n_delete(), if local { 1 } else { 0 }); // FolC was is renamed to an existing name, so it is not considered as renamed
    // There was 5 inserts
    assert_eq!(counter.n_get(), if local { 0 } else { 5 });
    assert_eq!(counter.n_put(), if local { 5 } else { 0 });
}

#[test]
fn test_deep_hierarchy() {
    // "remote"
    deep_hierarchy(false);
    // "local"
    deep_hierarchy(true);
}

#[test]
fn rename_on_both_sides() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // Test that renaming a file within a directory that was renamed on the other side actually do a rename.

    // 1) move the folder alphabetically before
    fake_folder.remote_modifier().rename("A/a1", "A/a1m");
    fake_folder.local_modifier().rename("A", "_A");
    fake_folder.local_modifier().rename("B/b1", "B/b1m");
    fake_folder.remote_modifier().rename("B", "_B");

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_remote_state(),
        fake_folder.current_local_state()
    );
    assert!(fake_folder.current_remote_state().find("_A/a1m").is_some());
    assert!(fake_folder.current_remote_state().find("_B/b1m").is_some());
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 2);
    counter.reset();

    // 2) move alphabetically after
    fake_folder.remote_modifier().rename("_A/a2", "_A/a2m");
    fake_folder.local_modifier().rename("_B/b2", "_B/b2m");
    fake_folder.local_modifier().rename("_A", "S/A");
    fake_folder.remote_modifier().rename("_B", "S/B");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_remote_state(),
        fake_folder.current_local_state()
    );
    assert!(fake_folder.current_remote_state().find("S/A/a2m").is_some());
    assert!(fake_folder.current_remote_state().find("S/B/b2m").is_some());
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 2);
}

#[test]
fn move_file_to_different_folder_on_both_sides() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    // Test that moving a file within to different folder on both side does the right thing.

    fake_folder.remote_modifier().rename("B/b1", "A/b1");
    fake_folder.local_modifier().rename("B/b1", "C/b1");

    fake_folder.local_modifier().rename("B/b2", "A/b2");
    fake_folder.remote_modifier().rename("B/b2", "C/b2");

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_remote_state(),
        fake_folder.current_local_state()
    );
    assert!(fake_folder.current_remote_state().find("A/b1").is_some());
    assert!(fake_folder.current_remote_state().find("C/b1").is_some());
    assert!(fake_folder.current_remote_state().find("A/b2").is_some());
    assert!(fake_folder.current_remote_state().find("C/b2").is_some());
    assert_eq!(counter.n_move(), 0); // Unfortunately, we can't really make a move in this case
    assert_eq!(counter.n_get(), 2);
    assert_eq!(counter.n_put(), 2);
    assert_eq!(counter.n_delete(), 0);
    counter.reset();
}

// Test that deletes don't run before renames
#[test]
fn test_rename_parallelism() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().mkdir("A");
    fake_folder.remote_modifier().insert("A/file", 64, b'W');
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);

    fake_folder.local_modifier().mkdir("B");
    fake_folder.local_modifier().rename("A/file", "B/file");
    fake_folder.local_modifier().remove("A");

    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
}

#[test]
fn test_rename_parallelism_with_blacklist() {
    let test_file_name = "blackListFile";
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().mkdir("A");
    fake_folder.remote_modifier().insert("A/file", 64, b'W');

    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);

    fake_folder
        .remote_modifier()
        .insert(test_file_name, 64, b'W');
    fake_folder.server_error_paths().append(test_file_name, 500); // will be blacklisted
    assert!(!fake_folder.sync_once());

    fake_folder.remote_modifier().mkdir("B");
    fake_folder.remote_modifier().rename("A/file", "B/file");
    fake_folder.remote_modifier().remove("A");

    assert!(!fake_folder.sync_once());
    let folder_a = fake_folder.current_local_state().find("A").cloned();
    assert!(folder_a.is_none());
}

#[test]
fn test_moved_with_error() {
    // Row "Vfs::Off" only: the "Vfs::WithSuffix" (and Windows-only
    // "Vfs::WindowsCfApi") rows need virtual files.
    let get_name = |s: &str| s.to_owned();
    let src = "folder/folderA/file.txt";
    let dest = "folder/folderB/file.txt";
    let mut fake_folder = FakeFolder::new(folder_a_b_tree());
    let mut sync_opts = fake_folder.sync_engine().sync_options().clone();
    sync_opts.parallel_network_jobs = 0;
    fake_folder.sync_engine().set_sync_options(sync_opts);

    assert_local_eq_remote(&fake_folder);

    fake_folder.server_error_paths().append(src, 403);
    fake_folder
        .local_modifier()
        .rename(&get_name(src), &get_name(dest));
    assert!(
        fake_folder
            .current_local_state()
            .find(get_name(src))
            .is_none()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find(get_name(dest))
            .is_some()
    );
    assert!(fake_folder.current_remote_state().find(src).is_some());
    assert!(fake_folder.current_remote_state().find(dest).is_none());

    // sync1 file gets detected as error, instruction is still NEW_FILE
    fake_folder.sync_once();

    // sync2 file is in error state, checkErrorBlacklisting sets instruction to IGNORED
    fake_folder.sync_once();

    assert!(
        fake_folder
            .current_local_state()
            .find(get_name(src))
            .is_some()
    );
    assert!(
        fake_folder
            .current_local_state()
            .find(get_name(dest))
            .is_none()
    );
    assert!(fake_folder.current_remote_state().find(src).is_some());
    assert!(fake_folder.current_remote_state().find(dest).is_none());
}

const SHA1_511: &str = "<oc:checksums><checksum>SHA1:22596363b3de40b06f981fb85d82312e8c0ed511</checksum></oc:checksums>";

fn insert_with_extra(ff: &FakeFolder, path: &str, extra: &str) {
    let mut r = ff.remote_modifier();
    r.insert(path, 64, b'W');
    let file = r.find_mut(path);
    assert!(file.is_some());
    file.unwrap().extra_dav_properties = extra.to_owned();
}

#[test]
fn test_rename_deep_hierarchy() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut r = fake_folder.remote_modifier();
        r.mkdir("FolA");
        r.mkdir("FolA/FolB2");
        r.mkdir("FolA/FolB");
        r.mkdir("FolA/FolB/FolC");
        r.mkdir("FolA/FolB/FolC/FolD");
        r.mkdir("FolA/FolB/FolC/FolD/FolE");
    }
    insert_with_extra(&fake_folder, "FolA/FileA.txt", SHA1_511);
    insert_with_extra(&fake_folder, "FolA/FolB/FileB.txt", SHA1_511);
    insert_with_extra(&fake_folder, "FolA/FolB/FolC/FileC.txt", SHA1_511);
    insert_with_extra(&fake_folder, "FolA/FolB/FolC/FolD/FileD.txt", SHA1_511);
    insert_with_extra(&fake_folder, "FolA/FolB/FolC/FolD/FolE/FileE.txt", SHA1_511);
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    fake_folder
        .remote_modifier()
        .rename("FolA/FolB", "FolA/FolB2/FolB");
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::DatabaseAndFilesystem, []);
    assert!(fake_folder.sync_once());

    insert_with_extra(
        &fake_folder,
        "FolA/FolB2/FolB/FolC/FolD/FileD2.txt",
        SHA1_511,
    );
    {
        let mut r = fake_folder.remote_modifier();
        r.append_byte("FolA/FolB2/FolB/FolC/FolD/FileD.txt");
        let file_d_moved = r.find_mut("FolA/FolB2/FolB/FolC/FolD/FileD.txt");
        assert!(file_d_moved.is_some());
        file_d_moved.unwrap().extra_dav_properties = "<oc:checksums><checksum>SHA1:22596363b3de40b06f981fb85d82312e8c0ed522</checksum></oc:checksums>".to_owned();
    }
    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(fake_folder.sync_once());

    assert_local_eq_remote(&fake_folder);
}

#[test]
fn test_rename_same_file_in_multiple_paths() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut r = fake_folder.remote_modifier();
        r.mkdir("FolderA");
        r.mkdir("FolderA/folderParent");
        r.mkdir("FolderB");
        r.mkdir("FolderB/folderChild");
        r.insert("FolderB/folderChild/FileA.txt", 64, b'W');
        r.mkdir("FolderC");
    }

    let (
        folder_child_in_folder_a_folder_file_id,
        folder_child_in_folder_a_etag,
        file_a_in_folder_a_folder_file_id,
        file_a_in_folder_a_etag,
    ) = {
        let mut r = fake_folder.remote_modifier();
        let folder_parent_file_info = r.find("FolderA/folderParent").unwrap();
        let folder_parent_shared_folder_file_id = folder_parent_file_info.file_id.clone();
        let folder_parent_shared_folder_etag = folder_parent_file_info.etag.clone();
        let folder_child_file_info = r.find("FolderB/folderChild").unwrap();
        let folder_child_file_id = folder_child_file_info.file_id.clone();
        let folder_child_etag = folder_child_file_info.etag.clone();
        let file_a_file_info = r.find("FolderB/folderChild/FileA.txt").unwrap();
        let file_a_file_id = file_a_file_info.file_id.clone();
        let file_a_etag = file_a_file_info.etag.clone();

        let folder_c_file_info = r.find_mut("FolderC").unwrap();
        folder_c_file_info.file_id = folder_parent_shared_folder_file_id;
        folder_c_file_info.etag = folder_parent_shared_folder_etag;
        (
            folder_child_file_id,
            folder_child_etag,
            file_a_file_id,
            file_a_etag,
        )
    };

    assert!(fake_folder.sync_once());

    assert_local_eq_remote(&fake_folder);

    {
        let mut r = fake_folder.remote_modifier();
        r.remove("FolderB/folderChild");
        r.mkdir("FolderA/folderParent/folderChild");
        r.insert("FolderA/folderParent/folderChild/FileA.txt", 64, b'W');
        r.mkdir("FolderC/folderChild");
        r.insert("FolderC/folderChild/FileA.txt", 64, b'W');

        let fi = r.find_mut("FolderA/folderParent/folderChild").unwrap();
        fi.file_id = folder_child_in_folder_a_folder_file_id.clone();
        fi.etag = folder_child_in_folder_a_etag.clone();

        let fi = r
            .find_mut("FolderA/folderParent/folderChild/FileA.txt")
            .unwrap();
        fi.file_id = file_a_in_folder_a_folder_file_id.clone();
        fi.etag = file_a_in_folder_a_etag.clone();

        let fi = r.find_mut("FolderC/folderChild").unwrap();
        fi.file_id = folder_child_in_folder_a_folder_file_id;
        fi.etag = folder_child_in_folder_a_etag;

        let fi = r.find_mut("FolderC/folderChild/FileA.txt").unwrap();
        fi.file_id = file_a_in_folder_a_folder_file_id;
        fi.etag = file_a_in_folder_a_etag;
    }

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(fake_folder.sync_once());

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(fake_folder.sync_once());

    assert_local_eq_remote(&fake_folder);
}

/// The common setup of the two group folder tests.
fn group_folder_setup() -> FakeFolder {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.account().set_server_version("29.0.2.0");

    fake_folder.remote_modifier().mkdir("FolA");
    {
        let mut r = fake_folder.remote_modifier();
        let group_folder_root = r.find_mut("FolA").unwrap();
        group_folder_root.extra_dav_properties =
            "<nc:is-mount-root>true</nc:is-mount-root>".to_owned();
        set_all_perm(
            group_folder_root,
            RemotePermissions::from_server_string("WDNVCKRMG"),
        );
        r.mkdir("FolA/FolB");
        r.mkdir("FolA/FolB/FolC");
        r.mkdir("FolA/FolB/FolC/FolD");
        r.mkdir("FolA/FolB/FolC/FolD/FolE");
        r.insert("FolA/FileA.txt", 64, b'W');
        r.insert("FolA/FolB/FileB.txt", 64, b'W');
        r.insert("FolA/FolB/FolC/FileC.txt", 64, b'W');
        r.insert("FolA/FolB/FolC/FolD/FileD.txt", 64, b'W');
        r.insert("FolA/FolB/FolC/FolD/FolE/FileE.txt", 64, b'W');
    }
    assert!(fake_folder.sync_once());
    fake_folder
}

fn insert_five_local_files(ff: &mut FakeFolder) {
    let l = ff.local_modifier();
    l.insert("FolA/FileA2.txt", 64, b'W');
    l.insert("FolA/FolB/FileB2.txt", 64, b'W');
    l.insert("FolA/FolB/FolC/FileC2.txt", 64, b'W');
    l.insert("FolA/FolB/FolC/FolD/FileD2.txt", 64, b'W');
    l.insert("FolA/FolB/FolC/FolD/FolE/FileE2.txt", 64, b'W');
}

#[test]
fn test_block_rename_top_folder_from_group_folder() {
    let mut fake_folder = group_folder_setup();

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    insert_five_local_files(&mut fake_folder);
    fake_folder.local_modifier().rename("FolA", "FolA_Renamed");
    assert!(fake_folder.sync_once());
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 10);
    assert_eq!(counter.n_move(), 0);
    assert_eq!(counter.n_mkcol(), 5);
}

#[test]
fn test_allow_rename_child_folder_from_group_folder() {
    let mut fake_folder = group_folder_setup();

    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    insert_five_local_files(&mut fake_folder);
    fake_folder
        .local_modifier()
        .rename("FolA/FolB", "FolA/FolB_Renamed");
    fake_folder
        .local_modifier()
        .rename("FolA/FileA.txt", "FolA/FileA_Renamed.txt");
    assert!(fake_folder.sync_once());
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 5);
    assert_eq!(counter.n_move(), 2);
    assert_eq!(counter.n_mkcol(), 0);
}

fn multiple_rename_setup() -> FakeFolder {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut r = fake_folder.remote_modifier();
        r.mkdir("root");
        r.mkdir("root/stable");
        r.mkdir("root/stable/folder to move");
        r.insert("root/stable/folder to move/index.txt", 64, b'W');
        r.mkdir("root/stable/move");
    }
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);
    fake_folder
}

#[test]
fn test_multiple_rename_from_server() {
    let mut fake_folder = multiple_rename_setup();

    {
        let mut r = fake_folder.remote_modifier();
        r.rename("root", "root test");
        r.rename("root test/stable/folder to move", "root test/stable/folder");
        r.rename("root test/stable/folder", "root test/stable/move/folder");
    }
    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    fake_folder.sync_engine().callbacks().about_to_propagate =
        Some(Box::new(|items: &SyncFileItemVector| {
            let mut root: Option<SyncFileItemPtr> = None;
            let mut stable: Option<SyncFileItemPtr> = None;
            let mut folder: Option<SyncFileItemPtr> = None;
            let mut file: Option<SyncFileItemPtr> = None;
            let mut mv: Option<SyncFileItemPtr> = None;
            for item in items {
                let f = item.borrow().file.clone();
                eprintln!("{f}");
                match f.as_str() {
                    "root" => root = Some(item.clone()),
                    "root test/stable" => stable = Some(item.clone()),
                    "root test/stable/move" => mv = Some(item.clone()),
                    "root/stable/folder to move" => folder = Some(item.clone()),
                    "root test/stable/move/folder/index.txt" => file = Some(item.clone()),
                    _ => {}
                }
            }

            for item in [root, stable, mv, folder, file] {
                let item = item.expect("item not found");
                let item = item.borrow();
                assert_eq!(item.instruction, Instruction::Rename, "{}", item.file);
                assert_eq!(item.direction, Direction::Down, "{}", item.file);
            }
        }));

    assert!(fake_folder.sync_once());
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 0);
    assert_eq!(counter.n_mkcol(), 0);
    assert_local_eq_remote(&fake_folder);
}

#[test]
fn test_multiple_rename_from_local() {
    let mut fake_folder = multiple_rename_setup();

    fake_folder.local_modifier().rename("root", "root test");
    fake_folder
        .local_modifier()
        .rename("root test/stable/folder to move", "root test/stable/folder");
    fake_folder
        .local_modifier()
        .rename("root test/stable/folder", "root test/stable/move/folder");
    let counter = OperationCounter::default();
    counter.install(&fake_folder);

    assert!(fake_folder.sync_once());
    assert_eq!(counter.n_delete(), 0);
    assert_eq!(counter.n_get(), 0);
    assert_eq!(counter.n_put(), 0);
    assert_eq!(counter.n_move(), 2);
    assert_eq!(counter.n_mkcol(), 0);
    assert_local_eq_remote(&fake_folder);
}

#[test]
fn test_rename_complex_scenario_no_record_leak() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    assert_local_eq_remote(&fake_folder);

    {
        let mut r = fake_folder.remote_modifier();
        r.mkdir("0");
        r.mkdir("0/00 without file");
        r.mkdir("0/00 without file/project");
        r.mkdir("0/00 without file/project/a");
        r.mkdir("0/00 without file/project/a/a with file");
        r.mkdir("0/00 without file/project/a/a without file");
        r.mkdir("0/00 without file/project/a/aa with file");
        r.mkdir("0/00 without file/project/00 with file");
        r.mkdir("0/00 without file/project/new without file");
        r.insert("0/00 without file/project/00 with file/test.md", 64, b'W');
        r.insert("0/00 without file/project/a/a with file/test.md", 64, b'W');
        r.insert("0/00 without file/project/a/aa with file/test.md", 64, b'W');
    }

    assert!(fake_folder.sync_once());

    assert_eq!(count_db_records(&fake_folder), 12);

    {
        let mut r = fake_folder.remote_modifier();
        r.rename("0/00 without file/project", "project tests");
        r.rename("project tests/a", "project tests/a empty");
        r.rename(
            "project tests/a empty/a with file",
            "project tests/a with file",
        );
        r.rename(
            "project tests/a empty/a without file",
            "project tests/a without file",
        );
        r.rename(
            "project tests/a empty/aa with file",
            "project tests/aa with file",
        );
        r.rename(
            "project tests/new without file",
            "project tests/new without file",
        );
        r.rename("0/00 without file", "project tests/00 without file");
        r.rename("0", "project tests/z 0 empty");
    }

    let journal = fake_folder.sync_journal_handle();
    fake_folder.sync_engine().callbacks().item_completed =
        Some(Box::new(move |_: &SyncFileItemPtr, _: ErrorCategory| {
            let mut items_counter = 0;
            let db_result = journal.get_files_below_path(b"", |_| items_counter += 1);

            assert!(db_result.is_ok());
            if items_counter > 12 {
                assert!(items_counter <= 12);
            }
        }));

    assert!(fake_folder.sync_once());

    assert_eq!(count_db_records(&fake_folder), 12);
}

#[test]
fn test_rename_file_that_exists_in_multiple_paths() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.set_server_override(|request, _| {
        if request.method().as_str() == "MOVE" {
            return Some(FakeReply::from(error_reply(507, Bytes::new())));
        }
        None
    });

    {
        let mut r = fake_folder.remote_modifier();
        r.mkdir("FolderA");
        r.mkdir("FolderA/folderParent");
        r.insert("FolderA/folderParent/FileA.txt", 64, b'W');
        r.mkdir("FolderB");
        r.mkdir("FolderB/folderChild");
        r.insert("FolderB/folderChild/FileA.txt", 64, b'W');
        r.mkdir("FolderC");

        let file_a_file_info = r.find("FolderA/folderParent/FileA.txt").unwrap();
        let file_a_in_folder_a_folder_file_id = file_a_file_info.file_id.clone();
        let file_a_in_folder_a_etag = file_a_file_info.etag.clone();
        let duplicated_file_a_file_info = r.find_mut("FolderB/folderChild/FileA.txt").unwrap();

        duplicated_file_a_file_info.file_id = file_a_in_folder_a_folder_file_id;
        duplicated_file_a_file_info.etag = file_a_in_folder_a_etag;
    }

    assert!(fake_folder.sync_once());

    assert_local_eq_remote(&fake_folder);

    fake_folder
        .local_modifier()
        .rename("FolderA/folderParent/FileA.txt", "FolderC/FileA.txt");

    eprintln!("{:?}", fake_folder.current_local_state());

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(!fake_folder.sync_once());

    eprintln!("{:?}", fake_folder.current_local_state());

    fake_folder
        .sync_engine()
        .set_local_discovery_options(LocalDiscoveryStyle::FilesystemOnly, []);
    assert!(fake_folder.sync_once());

    eprintln!("{:?}", fake_folder.current_local_state());
}

// PropagateLocalRename must clear the lock token and locked state.
#[test]
fn test_lock_token_cleared_on_server_initiated_rename() {
    let mut fake_folder = FakeFolder::new(d_file_tree());
    assert!(fake_folder.sync_once());
    seed_stale_lock(&fake_folder, "D/file.txt");

    fake_folder.remote_modifier().rename("D", "D2");
    assert!(fake_folder.sync_once());

    let moved = fake_folder
        .sync_journal()
        .get_file_record(b"D2/file.txt")
        .expect("getFileRecord");
    let moved = moved.unwrap_or_default();
    assert!(moved.is_valid());
    assert!(moved.lockstate.lock_token.is_empty());
    assert!(!moved.lockstate.locked);

    let old_state = fake_folder.current_remote_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), old_state);
}

// PropagateRemoteMove must clear the locked state.
#[test]
fn test_locked_state_cleared_on_client_initiated_rename() {
    let mut fake_folder = FakeFolder::new(d_file_tree());
    assert!(fake_folder.sync_once());
    seed_stale_lock(&fake_folder, "D/file.txt");

    fake_folder.local_modifier().rename("D", "D2");
    assert!(fake_folder.sync_once());

    let moved = db_record(&fake_folder, "D2/file.txt");
    assert!(moved.is_valid());
    assert!(moved.lockstate.lock_token.is_empty());
    assert!(!moved.lockstate.locked);

    let old_state = fake_folder.current_remote_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), old_state);
}

// A 412 upload error must schedule rediscovery.
#[test]
fn test_upload412_schedules_rediscovery() {
    let mut fake_folder = FakeFolder::new(d_file_tree());
    assert!(fake_folder.sync_once());

    fake_folder.local_modifier().append_byte("D/file.txt");
    fake_folder.server_error_paths().append("D/file.txt", 412);
    assert!(!fake_folder.sync_once()); // Upload fails with 412.

    // The 412 handler must invalidate the parent folder etag so the next sync
    // rediscovers it instead of trusting the DB.
    let parent = db_record(&fake_folder, "D");
    assert!(parent.is_valid());
    assert_eq!(parent.etag, b"_invalid_");

    // After clearing the error, the next sync must succeed.
    fake_folder.server_error_paths().clear();
    let _blacklist_wiped = fake_folder.sync_journal().wipe_error_blacklist();
    assert!(fake_folder.sync_once());
    assert_local_eq_remote(&fake_folder);

    let old_state = fake_folder.current_remote_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_local_state(), old_state);
}

// A 412 or 423 upload error must clear the whole lock state, not just the token.
#[test]
fn test_upload423_clears_stale_lock_state() {
    // FakeFolder default-constructs with performInitialSync=true, so the DB is
    // already populated with D/file.txt before seedStaleLock runs.
    let mut fake_folder = FakeFolder::new(d_file_tree());
    seed_stale_lock(&fake_folder, "D/file.txt");
    fake_folder.local_modifier().append_byte("D/file.txt");
    fake_folder.server_error_paths().append("D/file.txt", 423);
    // 423 → FileLocked in commonErrorHandling (which clears the DB lock), but
    // FileLocked is swallowed by PropagatorCompositeJob so the overall sync
    // reports success.
    assert!(fake_folder.sync_once());

    let cleared = db_record(&fake_folder, "D/file.txt");
    assert!(cleared.is_valid());
    assert!(cleared.lockstate.lock_token.is_empty());
    assert!(!cleared.lockstate.locked);

    fake_folder.server_error_paths().clear();
    let old_state = fake_folder.current_local_state();
    assert!(fake_folder.sync_once());
    assert_eq!(fake_folder.current_remote_state(), old_state);
}

// The journal cleanup after a failed local remove must not treat a sibling as a
// child: "A/BC" is not inside "A/B".
#[test]
fn test_deleted_dir_prefix_does_not_match_sibling() {
    assert!(is_path_inside_deleted_dir("A/B/child.txt", "A/B"));
    assert!(is_path_inside_deleted_dir("A/B/sub/child.txt", "A/B"));

    // Sibling sharing the prefix must not be skipped.
    assert!(!is_path_inside_deleted_dir("A/BC", "A/B"));
    assert!(!is_path_inside_deleted_dir("A/BC/child.txt", "A/B"));

    // The directory itself is not inside itself, and an empty deletedDir matches nothing.
    assert!(!is_path_inside_deleted_dir("A/B", "A/B"));
    assert!(!is_path_inside_deleted_dir("A/B/child.txt", ""));
}
