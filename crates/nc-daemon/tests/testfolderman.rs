// SPDX-FileCopyrightText: 2019 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream `test/testfolderman.cpp` and `test/testforcesyncnow.cpp`
// (nextcloud/desktop v34.0.5): the functions about the folder manager's
// scheduling. The ones about path validity are in the config tests; the
// share, end-to-end encryption and macOS file provider ones do not apply.

use std::sync::Arc;

use nc_daemon::event::Event;
use nc_daemon::folder::Definition;
use nc_daemon::folder_man::{FolderMan, FolderManSettings, NullSettingsStore, no_watcher};
use nc_dav::{Account, ServerUrl};
use nc_journal::csync::ItemType;
use nc_journal::journal::SyncJournalFileRecord;
use nc_testutils::{FakeFolder, FileInfo};
use tokio::sync::mpsc::UnboundedReceiver;

fn local<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(tokio::task::LocalSet::new().run_until(f))
}

fn folder_man() -> (FolderMan, UnboundedReceiver<Event>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let fm = FolderMan::new(
        &tx,
        FolderManSettings::default(),
        Box::new(NullSettingsStore),
        no_watcher(),
    );
    (fm, rx)
}

fn create_account(user_name: &str) -> Arc<Account> {
    let server = nc_testutils::FakeServer::new(FileInfo::default());
    let url = ServerUrl::parse(&format!("http://{user_name}@nextcloud.test")).unwrap();
    Arc::new(Account::new(url, Arc::new(server)))
}

/// `folderDefinition(path)` of upstream's testhelper.
fn folder_definition(path: &str) -> Definition {
    Definition {
        local_path: path.to_owned(),
        target_path: "/".to_owned(),
        alias: "1".to_owned(),
        ..Definition::default()
    }
}

#[test]
fn test_processing_file_ids_push_notification() {
    local(async {
        let (mut fm, _rx) = folder_man();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path().canonicalize().unwrap();
        for d in ["user1_root", "user1_subfolder", "user2_root"] {
            std::fs::create_dir(dir.join(d)).unwrap();
        }
        let dir_path = dir.to_string_lossy().into_owned();
        fm.add_fake_connected_account("user1", create_account("user1"));
        fm.add_fake_connected_account("user2", create_account("user2"));

        let mut add_folder_for_testing =
            |account: &str,
             alias: &str,
             local_path: &str,
             target_path: &str,
             root_file_id: i64,
             file_ids: &[i64]| {
                let definition = Definition {
                    alias: alias.to_owned(),
                    local_path: format!("{dir_path}{local_path}"),
                    target_path: target_path.to_owned(),
                    ..Definition::default()
                };
                let alias = fm.add_folder(account, definition).expect("folder");
                let folder = fm.folder_mut(&alias).unwrap();
                // Q_EMIT folder->syncEngine().rootFileIdReceived(rootFileId)
                folder.root_file_id_received_from_sync_engine(root_file_id);
                let journal = folder.journal();
                for file_id in file_ids {
                    let record = SyncJournalFileRecord {
                        file_id: format!("{file_id:08}oc123xyz987e").into_bytes(),
                        modtime: nc_sync::utility::current_secs_since_epoch(),
                        path: format!("item{file_id}").into_bytes(),
                        item_type: ItemType::File,
                        etag: b"etag".to_vec(),
                        ..SyncJournalFileRecord::default()
                    };
                    journal.set_file_record(&record).unwrap();
                }
            };
        add_folder_for_testing("user1", "0", "/user1_root", "/", 10, &[11, 12, 13, 50]);
        add_folder_for_testing(
            "user1",
            "1",
            "/user1_subfolder",
            "/subfolder",
            15,
            &[16, 17, 18, 50],
        );
        add_folder_for_testing("user2", "2", "/user2_root", "/", 20, &[21, 22, 23, 50]);

        let mut verify = |user: &str, file_ids: &[i64], expected: &[&str]| {
            fm.clear_schedule_queue();
            // the account received a push notification about for specific file ids
            fm.slot_process_file_ids_push_notification(user, file_ids);
            // expect the sync state for all folders of that account containing this file id to change
            let mut scheduled = fm.schedule_queue();
            scheduled.sort();
            assert_eq!(scheduled, expected, "user {user} file ids {file_ids:?}");
        };
        verify("user1", &[10], &["0"]);
        verify("user1", &[11], &["0"]);
        verify("user1", &[13, 11], &["0"]);
        verify("user1", &[15], &["1"]);
        verify("user1", &[16], &["1"]);
        verify("user1", &[18, 16], &["1"]);
        verify("user1", &[15, 11], &["0", "1"]);
        verify("user1", &[11, 16, 21], &["0", "1"]);
        verify("user1", &[50], &["0", "1"]);
        verify("user1", &[20, 21, 22, 23, 404], &[]);

        verify("user2", &[20], &["2"]);
        verify("user2", &[21], &["2"]);
        verify("user2", &[23, 21], &["2"]);
        verify("user2", &[11, 16, 21], &["2"]);
        verify("user2", &[50], &["2"]);
        verify("user2", &[10, 11, 17, 18, 404], &[]);
    });
}

#[test]
fn test_unload_and_delete_all_folders() {
    local(async {
        let (mut fm, _rx) = folder_man();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("folder1")).unwrap();
        std::fs::create_dir(dir.path().join("folder2")).unwrap();
        fm.add_fake_connected_account("0", create_account("admin"));
        let p = dir.path().to_string_lossy();
        let folder1 = fm.add_folder("0", folder_definition(&format!("{p}/folder1")));
        let folder2 = fm.add_folder("0", folder_definition(&format!("{p}/folder2")));
        assert!(folder1.is_some());
        assert!(folder2.is_some());
        assert_eq!(fm.map().len(), 2);
        // (the socket API unregistration of upstream does not apply)
        fm.unload_and_delete_all_folders();
        assert_eq!(fm.map().len(), 0);
    });
}

// test/testforcesyncnow.cpp: `User::forceSyncNow()` routes through
// `FolderMan::forceSyncForFolder`, which is what `ncsync sync-now` calls.
#[test]
fn force_sync_now_with_classic_folder_unpauses_and_schedules() {
    let fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    local(async {
        let (mut fm, _rx) = folder_man();
        fm.add_fake_connected_account("0", fake_folder.account().clone());
        let alias = fm
            .add_folder("0", folder_definition(&fake_folder.local_path()))
            .unwrap();
        let id = fm.folder(&alias).unwrap().id();
        // Put the folder in a state that `forceSyncForFolder` is supposed to clear.
        fm.set_folder_sync_paused(id, true);
        assert!(fm.folder(&alias).unwrap().sync_paused());
        fm.force_sync_for_folder(&alias);
        // `FolderMan::forceSyncForFolder` calls `setSyncPaused(false)`
        assert!(!fm.folder(&alias).unwrap().sync_paused());
        assert_eq!(fm.schedule_queue(), vec![alias]);
    });
}

#[test]
fn force_sync_now_without_folder_or_file_provider_does_not_crash() {
    local(async {
        let (mut fm, _rx) = folder_man();
        fm.add_fake_connected_account("0", create_account("testuser"));
        // No folder for this account: forcing a sync of an unknown folder
        // must not crash (pre-fix upstream called forceSyncForFolder(nullptr)).
        fm.force_sync_for_folder("does-not-exist");
        assert!(fm.map().is_empty());
    });
}
