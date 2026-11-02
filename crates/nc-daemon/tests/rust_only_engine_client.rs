// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! rust_only: the engine interface of a GUI (`nc_daemon::client`), both
//! ways. Remote: the event loop serves its control endpoint, a client on
//! another thread subscribes over the socket, sees a sync's events, and
//! pauses and resumes the folder. In process: the engine runs on its own
//! thread and is driven through the same interface.

use std::path::Path;
use std::time::{Duration, Instant};

use nc_daemon::client::{EngineClient, InProcess, Remote, Subscription, TryRecvError};
use nc_daemon::control::Request;
use nc_daemon::daemon::{self, DaemonHooks, NoHooks, ReloadResult};
use nc_daemon::event::Event;
use nc_daemon::folder::Definition;
use nc_daemon::folder_man::{FolderMan, FolderManSettings, NullSettingsStore};
use nc_daemon::sync_result::SyncStatus;
use nc_daemon::ui_event::{Snapshot, UiEvent};
use nc_testutils::{FakeFolder, FileInfo, FileModifier};
use tokio::sync::mpsc::UnboundedSender;

const TIMEOUT: Duration = Duration::from_secs(60);

/// A folder manager with the fake server's account and one folder "0".
fn folder_man(tx: &UnboundedSender<Event>, fake_folder: &FakeFolder) -> FolderMan {
    let mut settings = FolderManSettings {
        remote_poll_interval: Duration::from_secs(1),
        ..FolderManSettings::default()
    };
    settings.folder.minimum_file_age_for_upload = Duration::ZERO;
    let mut fm = FolderMan::new(
        tx,
        settings,
        Box::new(NullSettingsStore),
        nc_daemon::folder_man::no_watcher(),
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
    fm
}

/// Waits (polling, bounded) for an event matching `want`.
fn wait_event(sub: &mut Subscription, what: &str, want: impl Fn(&UiEvent) -> bool) -> UiEvent {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        match sub.try_recv() {
            Ok(e) if want(&e) => return e,
            Ok(_) => {}
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
            Err(TryRecvError::Closed) => panic!("the event stream ended waiting for {what}"),
        }
    }
    panic!("timed out waiting for {what}");
}

fn ask(engine: &dyn EngineClient, request: Request) -> serde_json::Value {
    engine.request(request).blocking_recv().unwrap()
}

/// What a GUI does with either engine: read the status, pause, resume.
fn pause_and_resume(engine: &dyn EngineClient, sub: &mut Subscription) {
    let status = ask(engine, Request::Status);
    let snapshot = Snapshot::from_response(&status).expect("a status snapshot");
    assert_eq!(snapshot.accounts[0].id, "0");
    assert_eq!(snapshot.folders[0].alias, "0");

    let r = ask(
        engine,
        Request::Pause {
            folder: Some("0".into()),
        },
    );
    assert_eq!(r["ok"], true, "{r}");
    wait_event(sub, "the pause", |e| {
        matches!(e, UiEvent::FolderStateChanged { folder }
            if folder.paused && folder.status == SyncStatus::Paused)
    });
    let r = ask(
        engine,
        Request::Resume {
            folder: Some("0".into()),
        },
    );
    assert_eq!(r["ok"], true, "{r}");
    wait_event(
        sub,
        "the resume",
        |e| matches!(e, UiEvent::FolderStateChanged { folder } if !folder.paused),
    );
    let st = Snapshot::from_response(&ask(engine, Request::Status)).unwrap();
    assert!(!st.folders[0].paused);

    // What only a configuration's hooks can do.
    let r = ask(engine, Request::Show);
    assert_eq!(r["ok"], false);
}

fn wait_listening(socket: &Path) {
    let deadline = Instant::now() + TIMEOUT;
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "the endpoint does not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn rust_only_remote_client_follows_a_sync_and_pauses_a_folder() {
    let fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let runtime_dir = tempfile::tempdir().unwrap();
    let socket = runtime_dir.path().join("ncsyncd").join("test.sock");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut fm = folder_man(&tx, &fake_folder);
        nc_daemon::control::serve(&socket, tx.clone(), fm.ui_sender()).unwrap();

        // The client side, on a thread of its own like a GUI's.
        let (subscribed_tx, subscribed_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<std::thread::Result<()>>();
        let client_socket = socket.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                wait_listening(&client_socket);
                let engine = Remote::new(&client_socket);
                let mut sub = engine.subscribe();
                let _ = subscribed_tx.send(());
                wait_event(&mut sub, "the downloaded file", |e| {
                    matches!(e, UiEvent::ItemCompleted { folder, item }
                        if folder == "0" && item.file == "A/remote" && item.direction == "Down")
                });
                wait_event(&mut sub, "the end of the sync", |e| {
                    matches!(e, UiEvent::FolderStateChanged { folder }
                        if folder.status == SyncStatus::Success && !folder.syncing)
                });
                pause_and_resume(&engine, &mut sub);
            }));
            let _ = done_tx.send(result);
        });

        let driver = {
            let tx = tx.clone();
            async move {
                // A remote change once the client listens: found by the
                // etag poll and downloaded.
                subscribed_rx.await.expect("the client subscribed");
                fake_folder.remote_modifier().insert("A/remote", 64, b'R');
                let result = tokio::time::timeout(TIMEOUT * 2, done_rx).await;
                tx.send(Event::Shutdown).unwrap();
                result
            }
        };
        let mut hooks = NoHooks;
        let (_, result) = tokio::join!(daemon::run(&mut fm, &tx, &mut rx, &mut hooks), driver);
        match result {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(panic))) => std::panic::resume_unwind(panic),
            Ok(Err(_)) => panic!("the client thread went away"),
            Err(_) => panic!("the client did not finish"),
        }
    });
}

/// The engine runs on its own thread; the account is on a closed port, so
/// its folder's syncs fail, which is enough to see the events go out and
/// the requests come in. (The fake server cannot be used here: it runs its
/// own runtime, and it is not `Send`.)
#[test]
fn rust_only_in_process_engine_obeys_requests_and_sends_events() {
    let engine = InProcess::start(|tx| {
        let url = nc_dav::ServerUrl::parse("http://127.0.0.1:9").unwrap();
        let transport =
            nc_dav::HttpTransport::new(&Default::default()).map_err(|e| e.to_string())?;
        let account =
            std::sync::Arc::new(nc_dav::Account::new(url, std::sync::Arc::new(transport)));
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut fm = FolderMan::new(
            tx,
            FolderManSettings::default(),
            Box::new(NullSettingsStore),
            nc_daemon::folder_man::no_watcher(),
        );
        fm.add_fake_connected_account("0", account);
        fm.add_folder_from_settings(
            "0",
            Definition {
                alias: "0".to_owned(),
                local_path: format!("{}/", dir.path().display()),
                journal_path: ".sync_test.db".to_owned(),
                target_path: "/".to_owned(),
                ..Definition::default()
            },
            true,
        )
        .ok_or("no folder")?;
        Ok((fm, Box::new(KeepDir(dir)) as Box<dyn DaemonHooks>))
    })
    .unwrap();
    let mut sub = engine.subscribe();
    wait_event(&mut sub, "the failed sync", |e| {
        matches!(e, UiEvent::FolderStateChanged { folder }
            if folder.status == SyncStatus::Error && !folder.syncing)
    });
    pause_and_resume(&engine, &mut sub);
    engine.shutdown();
    // The engine is gone: the stream ends.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match sub.try_recv() {
            Err(TryRecvError::Closed) => break,
            _ if Instant::now() > deadline => panic!("the stream did not end"),
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Hooks keeping the folder's directory alive.
struct KeepDir(#[allow(dead_code)] tempfile::TempDir);

impl DaemonHooks for KeepDir {
    fn reload(&mut self, _: &mut FolderMan, _: Option<&str>, _: &[String]) -> ReloadResult {
        ReloadResult::error("no configuration")
    }
}
