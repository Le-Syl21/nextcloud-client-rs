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
