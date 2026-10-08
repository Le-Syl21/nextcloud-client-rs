// SPDX-FileCopyrightText: 2019 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream `test/testremotewipe.cpp` (nextcloud/desktop v34.0.5).

//! The remote wipe: a revoked app password alone wipes nothing; a server
//! asking for a wipe gets the account's folders deleted (journal and local
//! files), the account removed and `index.php/core/wipe/success` called.
//!
//! Upstream replaces the `RemoteWipe`'s own `QNetworkAccessManager` with a
//! `FakeQNAM` sharing the override; here the wipe requests go through the
//! account's transport, the FakeFolder server, so the one override answers
//! both. `QTest::qWait(500)` is [`pump`]: the folder manager handles its
//! queue for 500 ms.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nc_daemon::event::Event;
use nc_daemon::folder::Definition;
use nc_daemon::folder_man::{FolderMan, FolderManSettings, FolderSettingsStore, no_watcher};
use nc_dav::NetworkError;
use nc_testutils::server::{json_reply, status_reply, with_override};
use nc_testutils::{FakeFolder, FakeReply, FileInfo};
use tokio::sync::mpsc::UnboundedReceiver;

/// Records what the folder manager removes from the settings.
#[derive(Clone, Default)]
struct RecordingStore {
    removed_accounts: Arc<Mutex<Vec<String>>>,
    removed_folders: Arc<Mutex<Vec<String>>>,
}

impl FolderSettingsStore for RecordingStore {
    fn save_folder(&mut self, _: &str, _: &Definition, _: bool) {}
    fn remove_folder(&mut self, _account_id: &str, alias: &str) {
        self.removed_folders.lock().unwrap().push(alias.to_owned());
    }
    fn remove_account(&mut self, account_id: &str) {
        self.removed_accounts
            .lock()
            .unwrap()
            .push(account_id.to_owned());
    }
}

/// `QTest::qWait(ms)`: handles the folder manager's events for `ms`.
async fn pump(fm: &mut FolderMan, rx: &mut UnboundedReceiver<Event>, ms: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(e)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        fm.handle_event(e);
    }
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

#[derive(Default)]
struct Switches {
    /// whether respond with 401 to requests
    revoke_app_password: bool,
    /// whether a remote wipe should be done
    do_wipe: bool,
    /// the wipe endpoints that were called
    calls: Vec<String>,
}

#[test]
fn test_remote_wipe() {
    // RemoteWipe also needs an account present in the AccountManager
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let store = RecordingStore::default();
    // RemoteWipe needs FolderMan for actually wiping local data
    let mut fm = local.block_on(&rt, async {
        let mut fm = FolderMan::new(
            &tx,
            FolderManSettings::default(),
            Box::new(store.clone()),
            no_watcher(),
        );
        fm.add_fake_connected_account("0", fake_folder.account().clone());
        // let FolderMan know about our sync folder
        assert!(
            fm.add_folder("0", folder_definition(&fake_folder.local_path()))
                .is_some()
        );
        fm
    });

    let switches = Arc::new(Mutex::new(Switches::default()));
    {
        let switches = switches.clone();
        fake_folder.set_server_override(move |request, _| {
            let mut sw = switches.lock().unwrap();
            if !sw.revoke_app_password {
                // App password not revoked
                return None;
            }
            let path = request.uri().path().to_owned();
            if request.method() == http::Method::DELETE
                && path.ends_with("/ocs/v2.php/core/apppassword")
            {
                // allow deletion of appPassword to succeed
                return Some(FakeReply::Response(json_reply(200, "{}")));
            }
            if sw.do_wipe {
                if path.ends_with("/index.php/core/wipe/check") {
                    sw.calls.push("check".to_owned());
                    return Some(FakeReply::Response(json_reply(200, r#"{"wipe": true}"#)));
                } else if path.ends_with("/index.php/core/wipe/success") {
                    sw.calls.push("success".to_owned());
                    return Some(FakeReply::Response(json_reply(200, "{}")));
                }
            }
            // Responding with unauthorised
            Some(FakeReply::Response(with_override(
                status_reply(http::StatusCode::UNAUTHORIZED),
                NetworkError::AuthenticationRequiredError,
                Some(401),
            )))
        });
    }

    let local_path = fake_folder.local_path();
    let local_folder_exists = || std::path::Path::new(&local_path).is_dir();

    // initial sync to ensure we've had a working connection
    assert!(fake_folder.sync_once());

    // just revoking the app password -> no remote wipe should be done
    switches.lock().unwrap().revoke_app_password = true;
    assert!(!fake_folder.sync_once());
    // Upstream's keychain gives an empty app password in tests, so it calls the
    // check slot directly; here the 401 of the sync also starts a check (with
    // the account's password), answered 401 too.
    local.block_on(&rt, async {
        pump(&mut fm, &mut rx, 200).await;
        fm.start_remote_wipe_check("0", "password");
        pump(&mut fm, &mut rx, 500).await;
    });
    // ensure the account was not removed and the sync folder is still present
    assert_eq!(fm.account_states().count(), 1);
    assert!(
        local_folder_exists(),
        "Local sync folder should exist as no wipe was requested"
    );
    assert!(store.removed_accounts.lock().unwrap().is_empty());
    // The credential failure signed the account out.
    assert!(fm.account_state("0").unwrap().is_signed_out());

    // hack for test: close the journal db of FakeFolder, as FolderMan::addFolder creates its own
    fake_folder.sync_journal().close();

    // server tells us to wipe the local data
    switches.lock().unwrap().do_wipe = true;
    // ensure folder exists before performing the wipe
    assert!(
        local_folder_exists(),
        "Local sync folder should exist before wiping"
    );
    local.block_on(&rt, async {
        fm.start_remote_wipe_check("0", "password");
        pump(&mut fm, &mut rx, 500).await;
    });
    // account should now be gone
    assert_eq!(fm.account_states().count(), 0);
    assert_eq!(
        *store.removed_accounts.lock().unwrap(),
        vec!["0".to_owned()]
    );
    // local folder should now be gone
    assert!(
        !local_folder_exists(),
        "Local sync folder should be removed after wiping"
    );
    assert!(fm.map().is_empty());
    // derived: the server was told about the successful wipe
    assert_eq!(
        switches.lock().unwrap().calls,
        vec!["check".to_owned(), "success".to_owned()]
    );
}

/// rust_only: a wipe request for an account without a folder: upstream's
/// `wipeDone(accountState, false)` (nothing was wiped), so the account
/// stays and the server is not told.
#[test]
fn rust_only_remote_wipe_without_folder_keeps_the_account() {
    let fake_folder = FakeFolder::new(FileInfo::default());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let store = RecordingStore::default();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let calls = calls.clone();
        fake_folder.set_server_override(move |request, _| {
            let path = request.uri().path().to_owned();
            if path.ends_with("/index.php/core/wipe/check") {
                calls.lock().unwrap().push("check".to_owned());
                return Some(FakeReply::Response(json_reply(200, r#"{"wipe": true}"#)));
            }
            if path.ends_with("/index.php/core/wipe/success") {
                calls.lock().unwrap().push("success".to_owned());
                return Some(FakeReply::Response(json_reply(200, "{}")));
            }
            None
        });
    }
    local.block_on(&rt, async {
        let mut fm = FolderMan::new(
            &tx,
            FolderManSettings::default(),
            Box::new(store.clone()),
            no_watcher(),
        );
        fm.add_fake_connected_account("0", fake_folder.account().clone());
        fm.start_remote_wipe_check("0", "password");
        pump(&mut fm, &mut rx, 500).await;
        assert_eq!(fm.account_states().count(), 1);
    });
    assert!(store.removed_accounts.lock().unwrap().is_empty());
    assert_eq!(*calls.lock().unwrap(), vec!["check".to_owned()]);
}

/// rust_only: an empty app password makes no check at all.
#[test]
fn rust_only_remote_wipe_empty_app_password_does_not_check() {
    let fake_folder = FakeFolder::new(FileInfo::default());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let calls = Arc::new(Mutex::new(0));
    {
        let calls = calls.clone();
        fake_folder.set_server_override(move |request, _| {
            if request.uri().path().contains("/core/wipe/") {
                *calls.lock().unwrap() += 1;
            }
            None
        });
    }
    local.block_on(&rt, async {
        let mut fm = FolderMan::new(
            &tx,
            FolderManSettings::default(),
            Box::new(RecordingStore::default()),
            no_watcher(),
        );
        fm.add_fake_connected_account("0", fake_folder.account().clone());
        fm.start_remote_wipe_check("0", "");
        pump(&mut fm, &mut rx, 200).await;
    });
    assert_eq!(*calls.lock().unwrap(), 0);
}
