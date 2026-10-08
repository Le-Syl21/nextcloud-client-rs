// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! rust_only: the daemon's event loop end to end against the FakeFolder
//! server: the initial sync of a loaded folder, a remote change found by
//! etag polling, a local change found by the inotify watcher, pause/resume
//! and sync-now over the control requests.

use std::time::Duration;

use nc_daemon::control::Request;
use nc_daemon::daemon::{self, NoHooks};
use nc_daemon::event::Event;
use nc_daemon::folder::Definition;
use nc_daemon::folder_man::{FolderMan, FolderManSettings, NullSettingsStore};
use nc_testutils::{FakeFolder, FileInfo, FileModifier};
use tokio::sync::mpsc::UnboundedSender;

async fn control(tx: &UnboundedSender<Event>, request: Request) -> serde_json::Value {
    let (otx, orx) = tokio::sync::oneshot::channel();
    tx.send(Event::Control(request, otx)).unwrap();
    orx.await.unwrap()
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn rust_only_daemon_polls_watches_and_obeys_control_requests() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut settings = FolderManSettings {
            remote_poll_interval: Duration::from_secs(1),
            ..FolderManSettings::default()
        };
        settings.folder.minimum_file_age_for_upload = Duration::ZERO;
        // the default exclude list (it excludes the watcher's test files)
        let exclude_dir = tempfile::tempdir().unwrap();
        let exclude = exclude_dir.path().join("sync-exclude.lst");
        std::fs::write(&exclude, nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST).unwrap();
        settings.folder.exclude_files = vec![exclude.to_string_lossy().into_owned()];
        let mut fm = FolderMan::new(
            &tx,
            settings,
            Box::new(NullSettingsStore),
            daemon::inotify_watcher_factory(),
        );
        fm.add_fake_connected_account("0", fake_folder.account().clone());
        let alias = fm
            .add_folder_from_settings(
                "0",
                Definition {
                    alias: "0".to_owned(),
                    local_path: fake_folder.local_path(),
                    journal_path: ".sync_test.db".to_owned(),
                    target_path: "/".to_owned(),
                    ..Definition::default()
                },
                true,
            )
            .unwrap();
        let driver = {
            let tx = tx.clone();
            async move {
                // the loaded folder is scheduled and syncs
                wait_for("the initial sync", || {
                    let status = status_of(&tx);
                    status.is_some()
                })
                .await;
                let st = control(&tx, Request::Status).await;
                assert_eq!(st["ok"], true);
                assert_eq!(st["folders"][0]["alias"], alias);

                // a remote change: found by the etag poll (1 s here)
                fake_folder.remote_modifier().insert("A/remote", 64, b'R');
                let local_file = fake_folder.local_dir().join("A/remote");
                wait_for("the remote file to be downloaded", || local_file.exists()).await;

                // a local change: found by the folder watcher
                fake_folder.local_modifier().insert("B/local", 32, b'L');
                wait_for("the local file to be uploaded", || {
                    fake_folder.current_remote_state().find("B/local").is_some()
                })
                .await;

                // pause: a remote change is no longer applied
                let r = control(&tx, Request::Pause { folder: None }).await;
                assert_eq!(r["ok"], true);
                let st = control(&tx, Request::Status).await;
                assert_eq!(st["folders"][0]["paused"], true);
                fake_folder
                    .remote_modifier()
                    .insert("C/while-paused", 16, b'P');
                tokio::time::sleep(Duration::from_secs(3)).await;
                assert!(!fake_folder.local_dir().join("C/while-paused").exists());

                // resume and sync now
                let r = control(&tx, Request::Resume { folder: None }).await;
                assert_eq!(r["ok"], true);
                let r = control(
                    &tx,
                    Request::SyncNow {
                        folder: Some(alias.clone()),
                    },
                )
                .await;
                assert_eq!(r["ok"], true);
                let paused_file = fake_folder.local_dir().join("C/while-paused");
                wait_for("the sync after resume", || paused_file.exists()).await;
                let r = control(
                    &tx,
                    Request::SyncNow {
                        folder: Some("nope".into()),
                    },
                )
                .await;
                assert_eq!(r["ok"], false);

                wait_for("idle", || true).await;
                tx.send(Event::Shutdown).unwrap();
                fake_folder
            }
        };
        let mut hooks = NoHooks;
        let (_, fake_folder) = tokio::join!(daemon::run(&mut fm, &tx, &mut rx, &mut hooks), driver);
        assert_eq!(
            fake_folder.current_local_state(),
            fake_folder.current_remote_state()
        );
    });
}

/// Whether a status request could be posted (the loop runs).
fn status_of(tx: &UnboundedSender<Event>) -> Option<()> {
    (!tx.is_closed()).then_some(())
}

/// rust_only: office lock files seen by the folder watcher lock and unlock
/// the document on the server (`slotLockedFilesFound` /
/// `slotFilesLockReleased`), when the account has the files locking
/// capability.
#[test]
fn rust_only_daemon_locks_documents_opened_in_an_office_application() {
    use std::sync::{Arc, Mutex};

    use nc_testutils::LockState;
    use nc_testutils::server::file_path_from_url;

    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({"files": {"locking": "1.0"}}));
    // The LOCK / UNLOCK requests. The fake server answers them with
    // `FakeFileLockReply` (a lock held by `admin`, the account's user) and,
    // unlike upstream's fake, also records the lock in the remote tree, as
    // a real server reports it in the next PROPFIND.
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let r = requests.clone();
    fake_folder.set_server_override(move |request, state| {
        let verb = request.method().as_str().to_owned();
        if verb != "LOCK" && verb != "UNLOCK" {
            return None;
        }
        let path = file_path_from_url(request.uri()).unwrap_or_default();
        r.lock().unwrap().push(format!("{verb} {path}"));
        if let Some(fi) = state.remote_root.find_invalidating_etags(path.as_str()) {
            if verb == "LOCK" {
                fi.lock_state = LockState::FileLocked;
                fi.lock_owner_id = "admin".to_owned();
                fi.lock_time = nc_sync::utility::current_secs_since_epoch() as u64;
                fi.lock_timeout = 1800;
            } else {
                fi.lock_state = LockState::FileUnlocked;
                fi.lock_owner_id.clear();
                fi.lock_time = 0;
                fi.lock_timeout = 0;
            }
        }
        None
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut settings = FolderManSettings {
            remote_poll_interval: Duration::from_secs(1),
            ..FolderManSettings::default()
        };
        settings.folder.minimum_file_age_for_upload = Duration::ZERO;
        let exclude_dir = tempfile::tempdir().unwrap();
        let exclude = exclude_dir.path().join("sync-exclude.lst");
        std::fs::write(&exclude, nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST).unwrap();
        settings.folder.exclude_files = vec![exclude.to_string_lossy().into_owned()];
        let mut fm = FolderMan::new(
            &tx,
            settings,
            Box::new(NullSettingsStore),
            daemon::inotify_watcher_factory(),
        );
        fm.add_fake_connected_account("0", fake_folder.account().clone());
        fm.add_folder_from_settings(
            "0",
            Definition {
                alias: "0".to_owned(),
                local_path: fake_folder.local_path(),
                journal_path: ".sync_test.db".to_owned(),
                target_path: "/".to_owned(),
                ..Definition::default()
            },
            true,
        )
        .unwrap();
        let driver = {
            let tx = tx.clone();
            async move {
                // a document, uploaded
                fake_folder
                    .local_modifier()
                    .insert("A/report.odt", 32, b'D');
                wait_for("the document to be uploaded", || {
                    fake_folder
                        .current_remote_state()
                        .find("A/report.odt")
                        .is_some()
                })
                .await;
                // (The daemon's folder holds the journal: it is not opened
                // here.)
                let count = |verb: &str| {
                    requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|r| r.starts_with(&format!("{verb} ")))
                        .count()
                };

                // the office application opens it: its lock file appears
                let lock_file = fake_folder.local_dir().join("A/.~lock.report.odt#");
                std::fs::write(&lock_file, b"lock").unwrap();
                wait_for("the document to be locked", || count("LOCK") > 0).await;
                // the sync after the lock sees it on the server
                wait_for("the lock to be synced", || {
                    fake_folder
                        .current_remote_state()
                        .find("A/report.odt")
                        .is_some()
                })
                .await;
                tokio::time::sleep(Duration::from_secs(2)).await;

                // the application closes it: the lock file goes away
                std::fs::remove_file(&lock_file).unwrap();
                wait_for("the document to be unlocked", || count("UNLOCK") > 0).await;
                assert_eq!(
                    *requests.lock().unwrap(),
                    ["LOCK A/report.odt", "UNLOCK A/report.odt"]
                );
                assert!(
                    fake_folder
                        .current_remote_state()
                        .find(".~lock.report.odt#")
                        .is_none()
                );

                tx.send(Event::Shutdown).unwrap();
                fake_folder
            }
        };
        let mut hooks = NoHooks;
        let (_, _fake_folder) =
            tokio::join!(daemon::run(&mut fm, &tx, &mut rx, &mut hooks), driver);
    });
}
