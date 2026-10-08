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

/// Reload hooks for the tests: the folders in `wipe` are removed with their
/// journal (`removeFolder`), the alias `"keep:<alias>"` unloads a folder and
/// keeps its journal (`unloadFolder`).
struct UnloadHooks;

impl daemon::DaemonHooks for UnloadHooks {
    fn reload(
        &mut self,
        fm: &mut FolderMan,
        _config: Option<&str>,
        wipe: &[String],
    ) -> daemon::ReloadResult {
        let mut waiting = Vec::new();
        for w in wipe {
            let (alias, wipe) = match w.strip_prefix("keep:") {
                Some(a) => (a, false),
                None => (w.as_str(), true),
            };
            waiting.extend(fm.unload_folder(alias, wipe));
        }
        daemon::ReloadResult {
            response: serde_json::json!({"ok": true}),
            waiting,
        }
    }
}

/// rust_only: `ncsync folder exclude|include` over the control socket
/// (`FolderStatusModel::slotApplySelectiveSync`): a newly excluded folder
/// loses its local files that are unchanged since the last sync and keeps
/// the changed ones (`ProcessDirectoryJob::processBlacklisted`), nothing is
/// deleted on the server, and an included folder is downloaded again.
/// Then a folder removed by a reload: its journal is wiped once its sync
/// is back, or kept for an unloaded folder.
#[test]
fn rust_only_daemon_selective_sync_and_folder_removal() {
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
        let exclude_dir = tempfile::tempdir().unwrap();
        let exclude = exclude_dir.path().join("sync-exclude.lst");
        std::fs::write(&exclude, nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST).unwrap();
        settings.folder.exclude_files = vec![exclude.to_string_lossy().into_owned()];
        let mut fm = FolderMan::new(
            &tx,
            settings,
            Box::new(NullSettingsStore),
            nc_daemon::folder_man::no_watcher(),
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
        let journal = fake_folder.local_dir().join(".sync_test.db");
        let driver = {
            let tx = tx.clone();
            let journal = journal.clone();
            async move {
                let idle = |st: &serde_json::Value| {
                    (st["folders"][0]["status"] == "Success"
                        || st["folders"][0]["status"] == "Problem")
                        && st["folders"][0]["syncing"] == false
                        && st["folders"][0]["scheduled"] == false
                };
                let wait_idle = async |what: &str| {
                    for _ in 0..300 {
                        if idle(&control(&tx, Request::Status).await) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    panic!("timed out waiting for {what}");
                };
                wait_idle("the initial sync").await;

                // pause, change A/a2 locally, exclude A
                assert_eq!(
                    control(&tx, Request::Pause { folder: None }).await["ok"],
                    true
                );
                fake_folder.local_modifier().append_byte("A/a2");
                let r = control(
                    &tx,
                    Request::SelectiveSync {
                        folder: alias.clone(),
                        exclude: vec!["/A".to_owned()],
                        include: Vec::new(),
                    },
                )
                .await;
                assert_eq!(r["ok"], true, "{r}");
                assert_eq!(r["blacklist"], serde_json::json!(["A/"]));
                assert_eq!(r["changes"], serde_json::json!(["A/"]));
                // a file cannot be excluded, nor a folder inside an excluded one included
                let r = control(
                    &tx,
                    Request::SelectiveSync {
                        folder: alias.clone(),
                        exclude: vec!["B/b1".to_owned()],
                        include: Vec::new(),
                    },
                )
                .await;
                assert_eq!(r["ok"], false, "{r}");
                let r = control(
                    &tx,
                    Request::SelectiveSync {
                        folder: fake_folder.local_path(),
                        exclude: Vec::new(),
                        include: vec!["A/sub".to_owned()],
                    },
                )
                .await;
                assert_eq!(r["ok"], false, "{r}");
                assert_eq!(
                    control(&tx, Request::Resume { folder: None }).await["ok"],
                    true
                );
                let a1 = fake_folder.local_dir().join("A/a1");
                wait_for("A/a1 to be removed locally", || !a1.exists()).await;
                wait_idle("the sync after the exclusion").await;
                // the changed file stays, nothing is deleted on the server
                assert!(fake_folder.local_dir().join("A/a2").exists());
                let remote = fake_folder.current_remote_state();
                assert!(remote.find("A/a1").is_some());
                assert_eq!(remote.find("A/a2").unwrap().size, 4);

                // include it again
                let r = control(
                    &tx,
                    Request::SelectiveSync {
                        folder: alias.clone(),
                        exclude: Vec::new(),
                        include: vec!["A".to_owned()],
                    },
                )
                .await;
                assert_eq!(r["ok"], true, "{r}");
                assert_eq!(r["blacklist"], serde_json::json!([]));
                assert_eq!(r["whitelist_added"], serde_json::json!(["A/"]));
                wait_for("A/a1 to be downloaded again", || a1.exists()).await;
                wait_idle("the sync after the inclusion").await;

                // unloaded (journal kept), while a sync may be starting
                assert_eq!(
                    control(&tx, Request::SyncNow { folder: None }).await["ok"],
                    true
                );
                let r = control(
                    &tx,
                    Request::Reload {
                        config: None,
                        wipe: vec![format!("keep:{alias}")],
                    },
                )
                .await;
                assert_eq!(r["ok"], true);
                let st = control(&tx, Request::Status).await;
                assert_eq!(st["folders"], serde_json::json!([]));
                assert!(journal.exists());
                tx.send(Event::Shutdown).unwrap();
                fake_folder
            }
        };
        let mut hooks = UnloadHooks;
        let (_, fake_folder) = tokio::join!(daemon::run(&mut fm, &tx, &mut rx, &mut hooks), driver);
        // The journal was closed: it opens again (no exclusive lock left).
        let db = nc_journal::journal::SyncJournalDb::new(&journal);
        assert!(
            db.get_selective_sync_list(nc_journal::journal::SelectiveSyncListType::WhiteList)
                .unwrap()
                .contains(&"A/".to_owned())
        );
        drop(db);
        drop(fake_folder);
    });
}

/// rust_only: a folder removed by a reload while its sync runs: the answer
/// waits for the engine, then the journal is gone (and not recreated).
#[test]
fn rust_only_daemon_removed_folder_journal_wiped_after_its_sync() {
    let fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    // a download that never ends: the sync is still running when the
    // folder is removed
    fake_folder.remote_modifier().insert("A/slow", 64, b'S');
    fake_folder.set_server_override(|request, _| {
        (request.method() == "GET" && request.uri().path().ends_with("/A/slow"))
            .then_some(nc_testutils::server::FakeReply::Hang)
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut fm = FolderMan::new(
            &tx,
            FolderManSettings::default(),
            Box::new(NullSettingsStore),
            nc_daemon::folder_man::no_watcher(),
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
        let journal = fake_folder.local_dir().join(".sync_test.db");
        let driver = {
            let tx = tx.clone();
            let journal = journal.clone();
            async move {
                for _ in 0..100 {
                    let st = control(&tx, Request::Status).await;
                    if st["folders"][0]["syncing"] == true {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let st = control(&tx, Request::Status).await;
                assert_eq!(st["folders"][0]["syncing"], true, "{st}");
                let r = control(
                    &tx,
                    Request::Reload {
                        config: None,
                        wipe: vec![alias.clone()],
                    },
                )
                .await;
                assert_eq!(r["ok"], true);
                assert!(!journal.exists());
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert!(!journal.exists());
                tx.send(Event::Shutdown).unwrap();
            }
        };
        let mut hooks = UnloadHooks;
        tokio::join!(daemon::run(&mut fm, &tx, &mut rx, &mut hooks), driver);
        drop(fake_folder);
    });
}
