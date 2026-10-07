// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testpermissions.cpp (nextcloud/desktop v34.0.5).

mod common;

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, SystemTime};

use nc_journal::journal::SyncJournalDb;
use nc_journal::remote_permissions::RemotePermissions;
use nc_sync::AnotherSyncNeeded::{ImmediateFollowUp, NoFollowUpSync};
use nc_sync::filesystem::{self, FilePermissionsRestore, FolderPermissions};
use nc_sync::{Instruction, SyncFileItem};
use nc_testutils::server::error_reply;
use nc_testutils::{
    EtagsAction, FakeFolder, FakeReply, FileInfo, FileModifier, ItemCompletedSpy, find_conflict,
};

/// `applyPermissionsFromName`: `_PERM_xxx_` in a name sets the permissions.
fn apply_permissions_from_name(info: &mut FileInfo) {
    // QRegularExpression("_PERM_([^_]*)_[^/]*$"): the leftmost "_PERM_"
    // followed by a '_'-free capture and a '_' (names have no '/').
    let mut from = 0;
    while let Some(i) = info.name[from..].find("_PERM_") {
        let capture_start = from + i + "_PERM_".len();
        if let Some(len) = info.name[capture_start..].find('_') {
            let captured = info.name[capture_start..capture_start + len].to_owned();
            info.permissions = RemotePermissions::from_server_string(&captured);
            break;
        }
        from += i + 1;
    }

    for sub in info.children.values_mut() {
        apply_permissions_from_name(sub);
    }
}

/// Check if the expected rows in the DB are non-empty. Note that in some cases they might be, then we cannot use this function
/// https://github.com/owncloud/client/issues/2038
fn assert_csync_journal_ok(journal: &SyncJournalDb) {
    // The DB is opened in locked mode: close to allow us to access.
    journal.close();

    let db = rusqlite::Connection::open_with_flags(
        journal.database_file_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open the journal read-only");
    let count: i64 = db
        .query_row(
            "SELECT count(*) from metadata where length(fileId) == 0",
            [],
            |r| r.get(0),
        )
        .expect("query the journal");
    assert_eq!(count, 0);
}

fn is_read_only_folder(path: &str) -> bool {
    filesystem::is_folder_read_only(path)
}

fn find_discovery_item(spy: &[SyncFileItem], path: &str) -> SyncFileItem {
    spy.iter()
        .find(|item| item.destination() == path)
        .cloned()
        .unwrap_or_default()
}

fn item_instruction(spy: &ItemCompletedSpy, path: &str, instr: Instruction) -> bool {
    let item = spy.find_item(path);
    assert_eq!(item.instruction, instr, "item {path}");
    item.instruction == instr
}

fn discovery_instruction(spy: &[SyncFileItem], path: &str, instr: Instruction) -> bool {
    let item = find_discovery_item(spy, path);
    assert_eq!(item.instruction, instr, "discovery item {path}");
    item.instruction == instr
}

fn set_all_perm(fi: &mut FileInfo, perm: RemotePermissions) {
    fi.permissions = perm;
    for sub_fi in fi.children.values_mut() {
        set_all_perm(sub_fi, perm);
    }
}

fn perms(s: &str) -> RemotePermissions {
    RemotePermissions::from_server_string(s)
}

/// `connect(&syncEngine, &SyncEngine::aboutToPropagate, [&discovery](auto v) { discovery = v; })`.
fn hook_discovery(fake_folder: &mut FakeFolder) -> Rc<RefCell<Vec<SyncFileItem>>> {
    let discovery = Rc::new(RefCell::new(Vec::new()));
    let d = discovery.clone();
    fake_folder.sync_engine().callbacks().about_to_propagate = Some(Box::new(move |v| {
        *d.borrow_mut() = v.iter().map(|i| i.borrow().clone()).collect();
    }));
    discovery
}

fn owner_mode(path: &str) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {path}: {e}"))
        .permissions()
        .mode()
}

/// `QFileInfo::absolutePath()` of `local_path + file`.
fn absolute_parent(fake_folder: &FakeFolder, file: &str) -> String {
    filesystem::parent_dir(&format!("{}{}", fake_folder.local_path(), file))
}

/// The `removeReadOnly` lambda of `t7pl`.
fn remove_read_only(fake_folder: &FakeFolder, file: &str) {
    let path = format!("{}{}", fake_folder.local_path(), file);
    let _enabler = FilePermissionsRestore::new(
        &absolute_parent(fake_folder, file),
        FolderPermissions::ReadWrite,
    );
    if !filesystem::is_dir(&path) {
        let result = filesystem::remove(&path);
        if let Err(error_string) = &result {
            eprintln!("fail to delete: {path} {error_string}");
        }
        assert!(result.is_ok());
    } else {
        let result = filesystem::remove_recursively(&path, &mut |_, _| {}, &mut Vec::new());
        if !result {
            eprintln!("fail to delete: {path}");
        }
        assert!(result);
    }
}

/// The `renameReadOnly` lambda of `t7pl`.
fn rename_read_only(fake_folder: &mut FakeFolder, relative_path: &str, destination: &str) {
    let source_parent = absolute_parent(fake_folder, relative_path);
    let _source_enabler = FilePermissionsRestore::new(&source_parent, FolderPermissions::ReadWrite);

    let destination_parent = absolute_parent(fake_folder, destination);
    let _destination_enabler =
        FilePermissionsRestore::new(&destination_parent, FolderPermissions::ReadWrite);

    let is_source_read_only = owner_mode(&source_parent) & 0o200 == 0;
    let is_destination_read_only = owner_mode(&destination_parent) & 0o200 == 0;
    let add_write = |p: &str| {
        let m = owner_mode(p);
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m | 0o200)).unwrap();
    };
    let remove_write = |p: &str| {
        let m = owner_mode(p);
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m & !0o200)).unwrap();
    };
    if is_source_read_only {
        add_write(&source_parent);
    }
    if is_destination_read_only {
        add_write(&destination_parent);
    }
    fake_folder
        .local_modifier()
        .rename(relative_path, destination);
    if is_source_read_only {
        remove_write(&source_parent);
    }
    if is_destination_read_only {
        remove_write(&destination_parent);
    }
}

/// The `insertReadOnly` lambda of `t7pl`.
fn insert_read_only(fake_folder: &mut FakeFolder, file: &str, file_size: i64) {
    let _enabler = FilePermissionsRestore::new(
        &absolute_parent(fake_folder, file),
        FolderPermissions::ReadWrite,
    );
    fake_folder.local_modifier().insert(file, file_size, b'W');
}

/// The `editReadOnly` lambda of `t7pl`.
fn edit_read_only(fake_folder: &mut FakeFolder, file: &str) {
    let path = format!("{}{}", fake_folder.local_path(), file);
    assert!(owner_mode(&path) & 0o200 == 0, "{file} is writable");
    filesystem::set_file_read_only(&path, false);
    fake_folder.local_modifier().append_byte(file);
}

fn t7pl_row(move_to_trash_enabled: bool) {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    let mut sync_options = fake_folder.sync_engine().sync_options().clone();
    sync_options.move_files_to_trash = move_to_trash_enabled;
    fake_folder.sync_engine().set_sync_options(sync_options);

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Some of this test depends on the order of discovery. With threading
    // that order becomes effectively random, but we want to make sure to test
    // all cases and thus disable threading.
    let mut sync_opts = fake_folder.sync_engine().sync_options().clone();
    sync_opts.parallel_network_jobs = 1;
    fake_folder.sync_engine().set_sync_options(sync_opts);

    const CANNOT_BE_MODIFIED_SIZE: i64 = 133;
    const CAN_BE_MODIFIED_SIZE: i64 = 144;

    //create some files
    let insert_in = |fake_folder: &FakeFolder, dir: &str| {
        let mut rm = fake_folder.remote_modifier();
        rm.insert(&format!("{dir}normalFile_PERM_GWVND_.data"), 100, b'W');
        rm.insert(&format!("{dir}cannotBeRemoved_PERM_GWVN_.data"), 101, b'W');
        rm.insert(&format!("{dir}canBeRemoved_PERM_GD_.data"), 102, b'W');
        rm.insert(
            &format!("{dir}cannotBeModified_PERM_GDVN_.data"),
            CANNOT_BE_MODIFIED_SIZE,
            b'A',
        );
        rm.insert(
            &format!("{dir}canBeModified_PERM_GW_.data"),
            CAN_BE_MODIFIED_SIZE,
            b'W',
        );
    };

    //put them in some directories
    fake_folder
        .remote_modifier()
        .mkdir("normalDirectory_PERM_CKDNVG_");
    insert_in(&fake_folder, "normalDirectory_PERM_CKDNVG_/");
    fake_folder
        .remote_modifier()
        .mkdir("readonlyDirectory_PERM_MG_");
    insert_in(&fake_folder, "readonlyDirectory_PERM_MG_/");
    fake_folder
        .remote_modifier()
        .mkdir("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_");
    fake_folder
        .remote_modifier()
        .mkdir("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_");
    fake_folder.remote_modifier().insert(
        "readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data",
        100,
        b'W',
    );
    apply_permissions_from_name(&mut fake_folder.remote_modifier());

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_csync_journal_ok(fake_folder.sync_journal());
    eprintln!("Do some changes and see how they propagate");

    //1. remove the file than cannot be removed
    //  (they should be recovered)
    fake_folder
        .local_modifier()
        .remove("normalDirectory_PERM_CKDNVG_/cannotBeRemoved_PERM_GWVN_.data");
    remove_read_only(
        &fake_folder,
        "readonlyDirectory_PERM_MG_/cannotBeRemoved_PERM_GWVN_.data",
    );

    //2. remove the file that can be removed
    //  (they should properly be gone)
    remove_read_only(
        &fake_folder,
        "normalDirectory_PERM_CKDNVG_/canBeRemoved_PERM_GD_.data",
    );
    remove_read_only(
        &fake_folder,
        "readonlyDirectory_PERM_MG_/canBeRemoved_PERM_GD_.data",
    );

    //3. Edit the files that cannot be modified
    //  (they should be recovered, and a conflict shall be created)
    edit_read_only(
        &mut fake_folder,
        "normalDirectory_PERM_CKDNVG_/cannotBeModified_PERM_GDVN_.data",
    );
    edit_read_only(
        &mut fake_folder,
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
    );

    //4. Edit other files
    //  (they should be uploaded)
    fake_folder
        .local_modifier()
        .append_byte("normalDirectory_PERM_CKDNVG_/canBeModified_PERM_GW_.data");
    fake_folder
        .local_modifier()
        .append_byte("readonlyDirectory_PERM_MG_/canBeModified_PERM_GW_.data");

    //5. Create a new file in a read write folder
    // (should be uploaded)
    fake_folder.local_modifier().insert(
        "normalDirectory_PERM_CKDNVG_/newFile_PERM_GWDNV_.data",
        106,
        b'W',
    );
    apply_permissions_from_name(&mut fake_folder.remote_modifier());

    //do the sync
    assert!(fake_folder.sync_once());
    assert_csync_journal_ok(fake_folder.sync_journal());
    let mut current_local_state = fake_folder.current_local_state();

    //1.
    // File should be recovered
    assert!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/cannotBeRemoved_PERM_GWVN_.data")
            .is_some()
    );
    assert_eq!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data")
            .unwrap()
            .size,
        CANNOT_BE_MODIFIED_SIZE
    );
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/cannotBeRemoved_PERM_GWVN_.data")
            .is_some()
    );

    //2.
    // File should be deleted
    assert!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/canBeRemoved_PERM_GD_.data")
            .is_none()
    );
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/canBeRemoved_PERM_GD_.data")
            .is_none()
    );

    //3.
    // File should be recovered
    assert_eq!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/cannotBeModified_PERM_GDVN_.data")
            .unwrap()
            .size,
        CANNOT_BE_MODIFIED_SIZE
    );
    // and conflict created
    let c1 = find_conflict(
        &current_local_state,
        "normalDirectory_PERM_CKDNVG_/cannotBeModified_PERM_GDVN_.data",
    );
    assert!(c1.is_some());
    let c1 = c1.unwrap();
    assert_eq!(c1.size, CANNOT_BE_MODIFIED_SIZE + 1);
    let c2 = find_conflict(
        &current_local_state,
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
    );
    assert!(c2.is_some());
    let c2 = c2.unwrap();
    assert_eq!(c2.size, CANNOT_BE_MODIFIED_SIZE + 1);
    // remove the conflicts for the next state comparison
    let (c1_path, c2_path) = (c1.path(), c2.path());
    fake_folder.local_modifier().remove(&c1_path);
    remove_read_only(&fake_folder, &c2_path);

    //4. File should be updated, that's tested by assertLocalAndRemoteDir
    assert_eq!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/canBeModified_PERM_GW_.data")
            .unwrap()
            .size,
        CAN_BE_MODIFIED_SIZE + 1
    );
    assert_eq!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/canBeModified_PERM_GW_.data")
            .unwrap()
            .size,
        CAN_BE_MODIFIED_SIZE + 1
    );

    //5.
    // the file should be in the server and local
    assert!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/newFile_PERM_GWDNV_.data")
            .is_some()
    );

    // Both side should still be the same
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Next test

    //6. Create a new file in a read only folder
    // (they should not be uploaded)
    insert_read_only(
        &mut fake_folder,
        "readonlyDirectory_PERM_MG_/newFile_PERM_GWDNV_.data",
        105,
    );

    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    // error: can't upload to readonly
    assert!(fake_folder.sync_once());

    assert_csync_journal_ok(fake_folder.sync_journal());
    current_local_state = fake_folder.current_local_state();

    // 6.
    //  The file cannot be uploaded to the read-only folder, but it MUST be kept locally:
    //  it is local-only data and silently deleting it would lose it (issues #7797/#10099).
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/newFile_PERM_GWDNV_.data")
            .is_some()
    );
    assert!(
        fake_folder
            .current_remote_state()
            .find("readonlyDirectory_PERM_MG_/newFile_PERM_GWDNV_.data")
            .is_none()
    );
    // Remove the kept local-only file so the following sections can compare local and remote.
    remove_read_only(
        &fake_folder,
        "readonlyDirectory_PERM_MG_/newFile_PERM_GWDNV_.data",
    );
    // Both side should still be the same
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    //######################################################################
    eprintln!("remove the read only directory");
    // -> It must be recovered
    remove_read_only(&fake_folder, "readonlyDirectory_PERM_MG_");
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    assert_csync_journal_ok(fake_folder.sync_journal());
    current_local_state = fake_folder.current_local_state();
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/cannotBeRemoved_PERM_GWVN_.data")
            .is_some()
    );
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_")
            .is_some()
    );
    // the subdirectory had delete permissions, but, it was within the recovered directory, so must also get recovered
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_")
            .is_some()
    );
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    //######################################################################
    eprintln!("move a directory in a outside read only folder");

    //Missing directory should be restored
    //new directory should be uploaded
    rename_read_only(
        &mut fake_folder,
        "readonlyDirectory_PERM_MG_/subdir_PERM_CKG_",
        "normalDirectory_PERM_CKDNVG_/subdir_PERM_CKDNVG_",
    );
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    current_local_state = fake_folder.current_local_state();

    // old name restored
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_")
            .is_some()
    );
    // contents moved (had move permissions)
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_")
            .is_none()
    );
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data")
            .is_none()
    );

    // new still exist  (and is uploaded)
    assert!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/subdir_PERM_CKDNVG_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data")
            .is_some()
    );

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // restore for further tests
    fake_folder
        .remote_modifier()
        .mkdir("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_");
    fake_folder.remote_modifier().insert(
        "readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data",
        64,
        b'W',
    );
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    //######################################################################
    eprintln!("rename a directory in a read only folder and move a directory to a read-only");

    // do a sync to update the database
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    assert_csync_journal_ok(fake_folder.sync_journal());

    assert!(
        fake_folder
            .current_local_state()
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data")
            .is_some()
    );

    //1. rename a directory in a read only folder
    //Missing directory should be restored
    //new directory should stay but not be uploaded
    rename_read_only(
        &mut fake_folder,
        "readonlyDirectory_PERM_MG_/subdir_PERM_CKG_",
        "readonlyDirectory_PERM_MG_/newname_PERM_CK_",
    );

    //2. move a directory from read to read only  (move the directory from previous step)
    rename_read_only(
        &mut fake_folder,
        "normalDirectory_PERM_CKDNVG_/subdir_PERM_CKDNVG_",
        "readonlyDirectory_PERM_MG_/moved_PERM_CK_",
    );

    // can't upload to readonly but not an error
    assert!(fake_folder.sync_once());
    current_local_state = fake_folder.current_local_state();

    //1.
    // old name restored
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_")
            .is_some()
    );
    // including contents
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/subdir_PERM_CKG_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data")
            .is_some()
    );
    // new no longer exists: it was a move of an existing (server-side) item into a read-only
    // folder, so the local copy is cleaned up while its data is restored at the source above.
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/newname_PERM_CK_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data")
            .is_none()
    );
    // but is not on server: should have been locally removed
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/newname_PERM_CK_")
            .is_none()
    );

    //2.
    // old removed
    assert!(
        current_local_state
            .find("normalDirectory_PERM_CKDNVG_/subdir_PERM_CKDNVG_")
            .is_none()
    );
    // but still on the server: the rename causing an error meant the deletes didn't execute
    assert!(
        fake_folder
            .current_remote_state()
            .find("normalDirectory_PERM_CKDNVG_/subdir_PERM_CKDNVG_")
            .is_some()
    );
    // new no longer exists
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/moved_PERM_CK_/subsubdir_PERM_CKDNVG_/normalFile_PERM_GWVND_.data")
            .is_none()
    );
    // should have been cleaned up as invalid item inside read-only folder
    assert!(
        current_local_state
            .find("readonlyDirectory_PERM_MG_/moved_PERM_CK_")
            .is_none()
    );
    fake_folder
        .remote_modifier()
        .remove("normalDirectory_PERM_CKDNVG_/subdir_PERM_CKDNVG_");

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    //######################################################################
    eprintln!("multiple restores of a file create different conflict files");

    fake_folder.remote_modifier().insert(
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
        64,
        b'W',
    );
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());

    edit_read_only(
        &mut fake_folder,
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
    );
    fake_folder.local_modifier().set_contents(
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
        b's',
    );
    //do the sync
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    assert_csync_journal_ok(fake_folder.sync_journal());

    edit_read_only(
        &mut fake_folder,
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
    );
    fake_folder.local_modifier().set_contents(
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
        b'd',
    );
    fake_folder.local_modifier().set_mod_time(
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
        SystemTime::now() + Duration::from_secs(24 * 3600),
    ); // make sure changes have different mtime

    //do the sync
    apply_permissions_from_name(&mut fake_folder.remote_modifier());
    assert!(fake_folder.sync_once());
    assert_csync_journal_ok(fake_folder.sync_journal());

    // there should be two conflict files
    current_local_state = fake_folder.current_local_state();
    let mut count = 0;
    while let Some(i) = find_conflict(
        &current_local_state,
        "readonlyDirectory_PERM_MG_/cannotBeModified_PERM_GDVN_.data",
    ) {
        assert!(i.content_char == b's' || i.content_char == b'd');
        let p = i.path();
        remove_read_only(&fake_folder, &p);
        current_local_state = fake_folder.current_local_state();
        count += 1;
    }
    assert_eq!(count, 2);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn t7pl() {
    // _data row "move to trash"
    t7pl_row(true);
    // _data row "delete"
    t7pl_row(false);
}

// What happens if the source can't be moved or the target can't be created?
#[test]
fn test_forbidden_moves() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    // Some of this test depends on the order of discovery. With threading
    // that order becomes effectively random, but we want to make sure to test
    // all cases and thus disable threading.
    let mut sync_opts = fake_folder.sync_engine().sync_options().clone();
    sync_opts.parallel_network_jobs = 1;
    fake_folder.sync_engine().set_sync_options(sync_opts);

    {
        let mut rm = fake_folder.remote_modifier();
        rm.mkdir("allowed");
        rm.mkdir("norename");
        rm.mkdir("nomove");
        rm.mkdir("nocreatefile");
        rm.mkdir("nocreatedir");
        rm.mkdir("zallowed"); // order of discovery matters

        rm.mkdir("allowed/sub");
        rm.mkdir("allowed/sub2");
        rm.insert("allowed/file", 64, b'W');
        rm.insert("allowed/sub/file", 64, b'W');
        rm.insert("allowed/sub2/file", 64, b'W');
        rm.mkdir("norename/sub");
        rm.insert("norename/file", 64, b'W');
        rm.insert("norename/sub/file", 64, b'W');
        rm.mkdir("nomove/sub");
        rm.insert("nomove/file", 64, b'W');
        rm.insert("nomove/sub/file", 64, b'W');
        rm.mkdir("zallowed/sub");
        rm.mkdir("zallowed/sub2");
        rm.insert("zallowed/file", 64, b'W');
        rm.insert("zallowed/sub/file", 64, b'W');
        rm.insert("zallowed/sub2/file", 64, b'W');

        set_all_perm(rm.find_mut("norename").unwrap(), perms("GWDVCK"));
        set_all_perm(rm.find_mut("nomove").unwrap(), perms("GWDNCK"));
        set_all_perm(rm.find_mut("nocreatefile").unwrap(), perms("GWDNVK"));
        set_all_perm(rm.find_mut("nocreatedir").unwrap(), perms("GWDNVC"));
    }

    assert!(fake_folder.sync_once());

    {
        let lm = fake_folder.local_modifier();
        // Renaming errors
        lm.rename("norename/file", "norename/file_renamed");
        lm.rename("norename/sub", "norename/sub_renamed");
        // Moving errors
        lm.rename("nomove/file", "allowed/file_moved");
        lm.rename("nomove/sub", "allowed/sub_moved");
        // Createfile errors
        lm.rename("allowed/file", "nocreatefile/file");
        lm.rename("zallowed/file", "nocreatefile/zfile");
        lm.rename("allowed/sub", "nocreatefile/sub"); // TODO: probably forbidden because it contains file children?
        // Createdir errors
        lm.rename("allowed/sub2", "nocreatedir/sub2");
        lm.rename("zallowed/sub2", "nocreatedir/zsub2");
    }

    // also hook into discovery!!
    let discovery = hook_discovery(&mut fake_folder);
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());
    let discovery = discovery.borrow().clone();

    // if renaming doesn't work, just delete+create
    assert!(item_instruction(
        &complete_spy,
        "norename/file",
        Instruction::Remove
    ));
    assert!(item_instruction(
        &complete_spy,
        "norename/sub",
        Instruction::Remove
    ));
    assert!(discovery_instruction(
        &discovery,
        "norename/sub",
        Instruction::Remove
    ));
    assert!(item_instruction(
        &complete_spy,
        "norename/file_renamed",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "norename/sub_renamed",
        Instruction::New
    ));
    // the contents can _move_
    assert!(item_instruction(
        &complete_spy,
        "norename/sub_renamed/file",
        Instruction::Rename
    ));

    // simiilarly forbidding moves becomes delete+create
    assert!(item_instruction(
        &complete_spy,
        "nomove/file",
        Instruction::Remove
    ));
    assert!(item_instruction(
        &complete_spy,
        "nomove/sub",
        Instruction::Remove
    ));
    assert!(discovery_instruction(
        &discovery,
        "nomove/sub",
        Instruction::Remove
    ));
    // nomove/sub/file is removed as part of the dir
    assert!(item_instruction(
        &complete_spy,
        "allowed/file_moved",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "allowed/sub_moved",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "allowed/sub_moved/file",
        Instruction::New
    ));

    // when moving to an invalid target, the targets should be ignored
    assert!(item_instruction(
        &complete_spy,
        "nocreatefile/file",
        Instruction::Ignore
    ));
    assert!(item_instruction(
        &complete_spy,
        "nocreatefile/zfile",
        Instruction::Ignore
    ));
    assert!(item_instruction(
        &complete_spy,
        "nocreatefile/sub",
        Instruction::Rename
    )); // TODO: What does a real server say?
    assert!(item_instruction(
        &complete_spy,
        "nocreatedir/sub2",
        Instruction::Ignore
    ));
    assert!(item_instruction(
        &complete_spy,
        "nocreatedir/zsub2",
        Instruction::Ignore
    ));

    // and the sources of the invalid moves should be restored, not deleted
    // (depending on the order of discovery a follow-up sync is needed)
    assert!(item_instruction(
        &complete_spy,
        "allowed/file",
        Instruction::None
    ));
    assert!(item_instruction(
        &complete_spy,
        "allowed/sub2",
        Instruction::None
    ));
    assert!(item_instruction(
        &complete_spy,
        "zallowed/file",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "zallowed/sub2",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "zallowed/sub2/file",
        Instruction::New
    ));
    assert_eq!(
        fake_folder.sync_engine().is_another_sync_needed(),
        ImmediateFollowUp
    );

    // A follow-up sync will restore allowed/file and allowed/sub2 and maintain the nocreatedir/file errors
    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert!(item_instruction(
        &complete_spy,
        "nocreatefile/file",
        Instruction::None
    ));
    assert!(item_instruction(
        &complete_spy,
        "nocreatefile/zfile",
        Instruction::None
    ));
    assert!(item_instruction(
        &complete_spy,
        "nocreatedir/sub2",
        Instruction::None
    ));
    assert!(item_instruction(
        &complete_spy,
        "nocreatedir/zsub2",
        Instruction::None
    ));

    assert!(item_instruction(
        &complete_spy,
        "allowed/file",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "allowed/sub2",
        Instruction::New
    ));
    assert!(item_instruction(
        &complete_spy,
        "allowed/sub2/file",
        Instruction::New
    ));

    let cls = fake_folder.current_local_state();
    assert!(cls.find("allowed/file").is_some());
    assert!(cls.find("allowed/sub2").is_some());
    assert!(cls.find("zallowed/file").is_some());
    assert!(cls.find("zallowed/sub2").is_some());
    assert!(cls.find("zallowed/sub2/file").is_some());
}

// A brand new local-only file dropped into a read-only folder cannot be uploaded.
// It must be ignored and kept on disk, not silently deleted, and a follow-up sync
// must be stable rather than looping (issues #7797/#10099).
#[test]
fn test_new_local_file_in_read_only_folder_is_kept() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().mkdir("readonly");
    fake_folder
        .remote_modifier()
        .insert("readonly/existing", 64, b'W');
    // No CanAddFile ('C') and no CanAddSubDirectories ('K'): nothing new may be created here.
    set_all_perm(
        fake_folder.remote_modifier().find_mut("readonly").unwrap(),
        perms("GWDNV"),
    );
    assert!(fake_folder.sync_once());

    // The user drops a new local-only file into the read-only folder.
    fake_folder
        .local_modifier()
        .insert("readonly/newlocal", 42, b'W');

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());

    // It is a genuine new file (no remote counterpart) so it is ignored, never removed,
    // and stays on disk. This is the move-branch's !isVirtualFile/IGNORE guard NOT firing.
    assert!(item_instruction(
        &complete_spy,
        "readonly/newlocal",
        Instruction::Ignore
    ));
    assert!(
        fake_folder
            .current_local_state()
            .find("readonly/newlocal")
            .is_some()
    );
    assert!(
        fake_folder
            .current_remote_state()
            .find("readonly/newlocal")
            .is_none()
    );

    // The second sync must not loop: the file is still ignored+kept and no follow-up is requested.
    complete_spy.clear();
    assert!(fake_folder.sync_once());
    assert!(item_instruction(
        &complete_spy,
        "readonly/newlocal",
        Instruction::Ignore
    ));
    assert!(
        fake_folder
            .current_local_state()
            .find("readonly/newlocal")
            .is_some()
    );
    assert_eq!(
        fake_folder.sync_engine().is_another_sync_needed(),
        NoFollowUpSync
    );
}

// A synced file moved into a read-only folder cannot land at its new location. The duplicate
// copy in the read-only folder must be cleaned up, while the original is restored from the
// server with its content byte-intact - the rejected move must never destroy data (#7797/#10099).
#[test]
fn test_move_synced_file_into_read_only_folder_restores_content() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    // Discovery order decides whether the restore lands on the first or a follow-up sync;
    // pin it so the test is deterministic.
    let mut sync_opts = fake_folder.sync_engine().sync_options().clone();
    sync_opts.parallel_network_jobs = 1;
    fake_folder.sync_engine().set_sync_options(sync_opts);

    fake_folder.remote_modifier().mkdir("allowed");
    fake_folder
        .remote_modifier()
        .insert("allowed/file", 200, b'X'); // distinctive content to prove it survives
    fake_folder.remote_modifier().mkdir("readonly");
    // No CanAddFile ('C') and no CanAddSubDirectories ('K'): nothing new may be created here.
    set_all_perm(
        fake_folder.remote_modifier().find_mut("readonly").unwrap(),
        perms("GWDNV"),
    );
    assert!(fake_folder.sync_once());

    // The user moves an already-synced file into the read-only folder.
    fake_folder
        .local_modifier()
        .rename("allowed/file", "readonly/file");

    assert!(fake_folder.sync_once());
    // The forbidden-move restore can need a follow-up sync to settle.
    if fake_folder.sync_engine().is_another_sync_needed() == ImmediateFollowUp {
        assert!(fake_folder.sync_once());
    }

    // The duplicate in the read-only folder is cleaned up locally and never reaches the server.
    assert!(
        fake_folder
            .current_local_state()
            .find("readonly/file")
            .is_none()
    );
    assert!(
        fake_folder
            .current_remote_state()
            .find("readonly/file")
            .is_none()
    );

    // The original is restored with its content intact (size and bytes), not just its path.
    let local_state = fake_folder.current_local_state();
    let restored = local_state.find("allowed/file");
    assert!(restored.is_some());
    let restored = restored.unwrap();
    assert_eq!(restored.size, 200);
    assert_eq!(restored.content_char, b'X');

    // Local and remote agree on everything, content included, and the sync has settled.
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(
        fake_folder.sync_engine().is_another_sync_needed(),
        NoFollowUpSync
    );
}

#[test]
fn test_parent_move_not_allowed_children_restored() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut rm = fake_folder.remote_modifier();
        rm.mkdir("forbidden-move");
        rm.mkdir("forbidden-move/sub1");
        rm.insert("forbidden-move/sub1/file1.txt", 100, b'W');
        rm.mkdir("forbidden-move/sub2");
        rm.insert("forbidden-move/sub2/file2.txt", 100, b'W');

        rm.find_mut("forbidden-move").unwrap().permissions = perms("WNCKG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder
        .local_modifier()
        .rename("forbidden-move", "forbidden-move-new");

    assert!(fake_folder.sync_once());

    // verify that original folder did not get wiped (files are still there)
    assert!(
        fake_folder
            .current_remote_state()
            .find("forbidden-move/sub1/file1.txt")
            .is_some()
    );
    assert!(
        fake_folder
            .current_remote_state()
            .find("forbidden-move/sub2/file2.txt")
            .is_some()
    );

    // verify that the duplicate folder has been created when trying to rename a folder that has its move permissions forbidden
    assert!(
        fake_folder
            .current_remote_state()
            .find("forbidden-move-new/sub1/file1.txt")
            .is_some()
    );
    assert!(
        fake_folder
            .current_remote_state()
            .find("forbidden-move-new/sub2/file2.txt")
            .is_some()
    );

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

/// `std::filesystem::status(path).permissions() & owner_read`. For a
/// missing path `status()` reports `perms::unknown` (all bits set), so the
/// check passes, as upstream's `ensureReadOnlyItem` does.
fn is_owner_readable(fake_folder: &FakeFolder, path: &str) -> bool {
    match std::fs::metadata(format!("{}{}", fake_folder.local_path(), path)) {
        Ok(m) => m.permissions().mode() & 0o400 != 0,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn is_owner_writable(fake_folder: &FakeFolder, path: &str) -> bool {
    owner_mode(&format!("{}{}", fake_folder.local_path(), path)) & 0o200 != 0
}

fn local_read_only(fake_folder: &FakeFolder, path: &str) -> bool {
    is_read_only_folder(&format!("{}{}", fake_folder.local_path(), path))
}

#[test]
fn test_read_only_folder_is_really_read_only() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("readOnlyFolder");
        remote.find_mut("readOnlyFolder").unwrap().permissions = perms("MG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(is_owner_readable(&fake_folder, "readOnlyFolder"));
}

#[test]
fn test_read_write_folder_is_really_read_write() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("readWriteFolder");
        remote.find_mut("readWriteFolder").unwrap().permissions = perms("GCKWDNVRSM");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(is_owner_readable(&fake_folder, "readWriteFolder"));
    assert!(is_owner_writable(&fake_folder, "readWriteFolder"));
}

#[test]
fn test_change_permissions_folder() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("testFolder");
        remote.find_mut("testFolder").unwrap().permissions = perms("MG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(local_read_only(&fake_folder, "testFolder"));

    fake_folder
        .remote_modifier()
        .find_mut("testFolder")
        .unwrap()
        .permissions = perms("CKWDNVRSMG");

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(!local_read_only(&fake_folder, "testFolder"));

    fake_folder
        .remote_modifier()
        .find_mut("testFolder")
        .unwrap()
        .permissions = perms("MG");

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(local_read_only(&fake_folder, "testFolder"));
}

#[test]
fn test_change_permissions_for_folder_hierarchy() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("testFolder");
        remote.find_mut("testFolder").unwrap().permissions = perms("MG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("testFolder/subFolderReadWrite");
        remote.mkdir("testFolder/subFolderReadOnly");
        remote
            .find_mut("testFolder/subFolderReadOnly")
            .unwrap()
            .permissions = perms("mG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(local_read_only(&fake_folder, "testFolder"));
    assert!(!local_read_only(
        &fake_folder,
        "testFolder/subFolderReadWrite"
    ));
    assert!(local_read_only(
        &fake_folder,
        "testFolder/subFolderReadOnly"
    ));

    {
        let mut remote = fake_folder.remote_modifier();
        remote
            .find_mut("testFolder/subFolderReadOnly")
            .unwrap()
            .permissions = perms("CKWDNVRSmG");
        remote
            .find_mut("testFolder/subFolderReadWrite")
            .unwrap()
            .permissions = perms("mG");
        remote.mkdir("testFolder/newSubFolder");
        remote.create("testFolder/testFile", 12, b'9');
        remote.create("testFolder/testReadOnlyFile", 13, b'8');
        remote
            .find_mut("testFolder/testReadOnlyFile")
            .unwrap()
            .permissions = perms("mG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(local_read_only(&fake_folder, "testFolder"));
    assert!(local_read_only(
        &fake_folder,
        "testFolder/subFolderReadWrite"
    ));
    assert!(!local_read_only(
        &fake_folder,
        "testFolder/subFolderReadOnly"
    ));

    {
        let mut remote = fake_folder.remote_modifier();
        remote.rename(
            "testFolder/subFolderReadOnly",
            "testFolder/subFolderReadWriteNew",
        );
        remote.rename(
            "testFolder/subFolderReadWrite",
            "testFolder/subFolderReadOnlyNew",
        );
        remote.rename("testFolder/testFile", "testFolder/testFileNew");
        remote.rename(
            "testFolder/testReadOnlyFile",
            "testFolder/testReadOnlyFileNew",
        );
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert!(local_read_only(&fake_folder, "testFolder"));
}

#[test]
fn test_delete_child_items_in_read_only_folder() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("readOnlyFolder");
        remote.mkdir("readOnlyFolder/test");
        remote.insert("readOnlyFolder/readOnlyFile.txt", 64, b'W');

        remote.find_mut("readOnlyFolder").unwrap().permissions = perms("MG");
        remote.find_mut("readOnlyFolder/test").unwrap().permissions = perms("mG");
        remote
            .find_mut("readOnlyFolder/readOnlyFile.txt")
            .unwrap()
            .permissions = perms("mG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    {
        let mut remote = fake_folder.remote_modifier();
        remote.remove("readOnlyFolder/test");
        remote.remove("readOnlyFolder/readOnlyFile.txt");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(is_owner_readable(&fake_folder, "readOnlyFolder"));
}

#[test]
fn test_rename_child_items_in_read_only_folder() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("readOnlyFolder");
        remote.mkdir("readOnlyFolder/test");
        remote.insert("readOnlyFolder/readOnlyFile.txt", 64, b'W');

        remote.find_mut("readOnlyFolder").unwrap().permissions = perms("MG");
        remote.find_mut("readOnlyFolder/test").unwrap().permissions = perms("mG");
        remote
            .find_mut("readOnlyFolder/readOnlyFile.txt")
            .unwrap()
            .permissions = perms("mG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    {
        let mut remote = fake_folder.remote_modifier();
        remote.rename("readOnlyFolder/test", "readOnlyFolder/testRename");
        remote.rename(
            "readOnlyFolder/readOnlyFile.txt",
            "readOnlyFolder/readOnlyFileRename.txt",
        );
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(is_owner_readable(&fake_folder, "readOnlyFolder"));
    assert!(is_owner_readable(&fake_folder, "readOnlyFolder/testRename"));
    assert!(is_owner_readable(
        &fake_folder,
        "readOnlyFolder/readOnlyFileRename.txt"
    ));
}

#[test]
fn test_move_child_items_in_read_only_folder() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("readOnlyFolder");
        remote.mkdir("readOnlyFolder/child");
        remote.mkdir("readOnlyFolder/test");
        remote.insert("readOnlyFolder/readOnlyFile.txt", 64, b'W');

        remote.find_mut("readOnlyFolder").unwrap().permissions = perms("MG");
        remote.find_mut("readOnlyFolder/child").unwrap().permissions = perms("mG");
        remote.find_mut("readOnlyFolder/test").unwrap().permissions = perms("mG");
        remote
            .find_mut("readOnlyFolder/readOnlyFile.txt")
            .unwrap()
            .permissions = perms("mG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    {
        let mut remote = fake_folder.remote_modifier();
        remote.rename("readOnlyFolder/test", "readOnlyFolder/child/test");
        remote.rename(
            "readOnlyFolder/readOnlyFile.txt",
            "readOnlyFolder/child/readOnlyFile.txt",
        );
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(is_owner_readable(&fake_folder, "readOnlyFolder"));
    assert!(is_owner_readable(&fake_folder, "readOnlyFolder/child"));
    assert!(is_owner_readable(&fake_folder, "readOnlyFolder/child/test"));
    assert!(is_owner_readable(
        &fake_folder,
        "readOnlyFolder/child/readOnlyFile.txt"
    ));
}

#[test]
fn test_modify_child_items_in_read_only_folder() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("readOnlyFolder");
        remote.mkdir("readOnlyFolder/test");
        remote.insert("readOnlyFolder/readOnlyFile.txt", 64, b'W');

        remote.find_mut("readOnlyFolder").unwrap().permissions = perms("MG");
        remote.find_mut("readOnlyFolder/test").unwrap().permissions = perms("mG");
        remote
            .find_mut("readOnlyFolder/readOnlyFile.txt")
            .unwrap()
            .permissions = perms("mG");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    {
        let mut remote = fake_folder.remote_modifier();
        remote.insert("readOnlyFolder/test/newFile.txt", 64, b'W');
        remote
            .find_mut("readOnlyFolder/test/newFile.txt")
            .unwrap()
            .permissions = perms("mG");
        remote.mkdir("readOnlyFolder/test/newFolder");
        remote
            .find_mut("readOnlyFolder/test/newFolder")
            .unwrap()
            .permissions = perms("mG");
        remote.append_byte("readOnlyFolder/readOnlyFile.txt");
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert!(is_owner_readable(&fake_folder, "readOnlyFolder"));
    assert!(is_owner_readable(&fake_folder, "readOnlyFolder/test"));
    assert!(is_owner_readable(
        &fake_folder,
        "readOnlyFolder/readOnlyFile.txt"
    ));
    assert!(is_owner_readable(
        &fake_folder,
        "readOnlyFolder/test/newFile.txt"
    ));
    // Upstream checks "readOnlyFolder/newFolder", which does not exist (the
    // folder was created in "readOnlyFolder/test").
    assert!(is_owner_readable(&fake_folder, "readOnlyFolder/newFolder"));
}

const DOWNLOAD_FORBIDDEN_MESSAGE: &str =
    "Access to this shared resource has been denied because its download permission is disabled.";

#[test]
fn test_forbidden_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.set_server_override(|req, _| {
        if req.method() == http::Method::GET {
            return Some(FakeReply::Response(error_reply(
                403,
                DOWNLOAD_FORBIDDEN_MESSAGE.into(),
            )));
        }
        None
    });

    fake_folder.remote_modifier().insert("file", 64, b'W');
    fake_folder.remote_modifier().mkdir("folder");
    fake_folder
        .remote_modifier()
        .insert("folder/file", 64, b'W');

    let set_download_forbidden_share = |fake_folder: &FakeFolder, relative_path: &str| {
        let mut rm = fake_folder.remote_modifier();
        let file_info = rm.find_mut(relative_path).expect("file info");
        file_info.is_shared = true;
        file_info.download_forbidden = true;
    };

    set_download_forbidden_share(&fake_folder, "file");
    set_download_forbidden_share(&fake_folder, "folder");
    set_download_forbidden_share(&fake_folder, "folder/file");

    // also hook into discovery!!
    let discovery = hook_discovery(&mut fake_folder);
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());
    let discovery = discovery.borrow().clone();

    assert!(item_instruction(&complete_spy, "file", Instruction::Ignore));
    assert!(discovery_instruction(
        &discovery,
        "file",
        Instruction::Ignore
    ));
    assert!(item_instruction(
        &complete_spy,
        "folder",
        Instruction::Ignore
    ));
    assert!(discovery_instruction(
        &discovery,
        "folder",
        Instruction::Ignore
    ));
}

#[test]
fn test_existing_file_become_forbidden_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.remote_modifier().insert("file", 64, b'W');
    fake_folder.remote_modifier().mkdir("folder");
    fake_folder
        .remote_modifier()
        .insert("folder/file", 64, b'W');

    let set_shared = |fake_folder: &FakeFolder, relative_path: &str| {
        let mut rm = fake_folder.remote_modifier();
        let file_info = rm.find_mut(relative_path).expect("file info");
        file_info.is_shared = true;
    };

    set_shared(&fake_folder, "file");
    set_shared(&fake_folder, "folder");
    set_shared(&fake_folder, "folder/file");

    assert!(fake_folder.sync_once());

    fake_folder.set_server_override(|req, _| {
        if req.method() == http::Method::GET {
            return Some(FakeReply::Response(error_reply(
                403,
                DOWNLOAD_FORBIDDEN_MESSAGE.into(),
            )));
        }
        None
    });

    let set_download_forbidden = |fake_folder: &FakeFolder, relative_path: &str| {
        let mut rm = fake_folder.remote_modifier();
        let file_info = rm
            .find_with(relative_path, EtagsAction::Invalidate)
            .expect("file info");
        file_info.is_shared = true;
        file_info.download_forbidden = true;
    };

    set_download_forbidden(&fake_folder, "file");
    set_download_forbidden(&fake_folder, "folder");
    set_download_forbidden(&fake_folder, "folder/file");

    // also hook into discovery!!
    let discovery = hook_discovery(&mut fake_folder);
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());
    let discovery = discovery.borrow().clone();

    assert!(item_instruction(&complete_spy, "file", Instruction::Remove));
    assert!(discovery_instruction(
        &discovery,
        "file",
        Instruction::Remove
    ));
    assert!(item_instruction(
        &complete_spy,
        "folder",
        Instruction::Remove
    ));
    assert!(discovery_instruction(
        &discovery,
        "folder",
        Instruction::Remove
    ));
    assert!(item_instruction(
        &complete_spy,
        "folder/file",
        Instruction::None
    ));
    assert!(discovery_instruction(
        &discovery,
        "folder/file",
        Instruction::Remove
    ));
}

#[test]
fn test_changing_permissions_without_etag_change() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.set_server_version("27.0.0");

    {
        let mut rm = fake_folder.remote_modifier();
        rm.mkdir("groupFolder");
        rm.mkdir("groupFolder/simpleChildFolder");
        rm.insert("groupFolder/simpleChildFolder/otherFile", 64, b'W');
        rm.mkdir("groupFolder/folderParent");
        rm.mkdir("groupFolder/folderParent/childFolder");
        rm.insert("groupFolder/folderParent/childFolder/file", 64, b'W');

        let group_folder_root = rm.find_mut("groupFolder").unwrap();
        set_all_perm(group_folder_root, perms("WDNVCKRMG"));
    }

    let propfind_counter = Arc::new(AtomicI32::new(0));

    {
        let propfind_counter = propfind_counter.clone();
        fake_folder.set_server_override(move |req, _| {
            if req.method().as_str() == "PROPFIND" {
                propfind_counter.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    }

    assert!(fake_folder.sync_once());
    assert_eq!(propfind_counter.load(Ordering::SeqCst), 5);

    fake_folder.set_server_version("31.0.0");

    {
        let mut rm = fake_folder.remote_modifier();
        let group_folder_root2 = rm.find_mut("groupFolder").unwrap();
        group_folder_root2.extra_dav_properties =
            "<nc:is-mount-root>true</nc:is-mount-root>".to_owned();

        rm.insert("groupFolder/simpleChildFolder/otherFile", 12, b'W');
    }

    assert!(fake_folder.sync_once());
    assert_eq!(propfind_counter.load(Ordering::SeqCst), 10);
}

#[test]
fn test_folder_readonly_when_remote_permissions_without_etag_changed() {
    let rows: &[(&str, bool)] = &[
        ("G", true),
        ("GS", true),
        ("GR", true),
        ("GRS", true),
        ("GRM", true),
        ("GRMS", true),
        ("GW", false),
        ("GWS", false),
        ("GD", false),
        ("GDS", false),
        ("GN", false),
        ("GV", false),
        ("GC", false),
        ("GK", false),
        ("GWD", false),
        ("GWDS", false),
        ("GDNCV", false),
        ("GDNCVS", false),
        ("GDNVCKR", false),
        ("GDNVCKRS", false),
    ];
    for &(remote_perm, is_read_only) in rows {
        // _data row named after remote_perm
        let remote_perm_change = if is_read_only {
            format!("{remote_perm}W")
        } else {
            "G".to_owned()
        };

        let mut fake_folder = FakeFolder::new(FileInfo::default());

        {
            let mut rm = fake_folder.remote_modifier();
            rm.mkdir("root");
            rm.mkdir("root/subdir");
            rm.mkdir("root/subdir/subsubdir");
        }

        eprintln!("initial sync with {remote_perm}");
        set_all_perm(
            fake_folder.remote_modifier().find_mut("root").unwrap(),
            perms(remote_perm),
        );
        assert!(fake_folder.sync_once());
        assert_eq!(
            local_read_only(&fake_folder, "root"),
            is_read_only,
            "{remote_perm}"
        );
        assert_eq!(
            local_read_only(&fake_folder, "root/subdir"),
            is_read_only,
            "{remote_perm}"
        );
        assert_eq!(
            local_read_only(&fake_folder, "root/subdir/subsubdir"),
            is_read_only,
            "{remote_perm}"
        );

        eprintln!("remote update with {remote_perm_change}");
        set_all_perm(
            fake_folder.remote_modifier().find_mut("root").unwrap(),
            perms(&remote_perm_change),
        );
        assert!(fake_folder.sync_once());
        assert_eq!(
            local_read_only(&fake_folder, "root"),
            !is_read_only,
            "{remote_perm}"
        );
        assert_eq!(
            local_read_only(&fake_folder, "root/subdir"),
            !is_read_only,
            "{remote_perm}"
        );
        assert_eq!(
            local_read_only(&fake_folder, "root/subdir/subsubdir"),
            !is_read_only,
            "{remote_perm}"
        );
    }
}
