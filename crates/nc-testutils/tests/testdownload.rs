// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2019 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testdownload.cpp (nextcloud/desktop v34.0.5).

use std::cell::{Cell, RefCell};
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use nc_dav::{Body, NetworkError, ReplyOverride};
use nc_sync::propagator::PropagatorEvent;
use nc_sync::{ProgressStatus, Status, SyncFileItem};
use nc_testutils::server::{error_reply, file_path_from_url, get_reply};
use nc_testutils::{FakeFolder, FakeReply, FileInfo, FileModifier, ItemCompletedSpy};

const STOP_AFTER: i64 = 3_123_668;

/* A FakeGetReply that sends max 'fakeSize' bytes, but whose ContentLength has the correct size */
fn broken_fake_get_reply(remote_root: &FileInfo, request: &nc_dav::Request) -> FakeReply {
    let file_name = file_path_from_url(request.uri()).unwrap_or_default();
    let reply = get_reply(remote_root, &file_name);
    let fake_size = STOP_AFTER.min(remote_root.find(file_name.as_str()).unwrap().size);
    let payload = remote_root.find(file_name.as_str()).unwrap().content_char;
    let (parts, _) = reply.into_parts();
    FakeReply::Response(http::Response::from_parts(
        parts,
        Body::from(vec![payload; fake_size as usize]),
    ))
}

/// `getItem(spy, path)`, which must exist.
fn get_item(spy: &ItemCompletedSpy, path: &str) -> SyncFileItem {
    let item = spy.items().into_iter().find(|i| i.destination() == path);
    assert!(item.is_some(), "no completed item for {path}");
    item.unwrap()
}

fn path_ends_with(request: &nc_dav::Request, suffix: &str) -> bool {
    file_path_from_url(request.uri()).is_some_and(|p| p.ends_with(suffix))
}

#[test]
fn test_resume() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.sync_engine().set_ignore_hidden_files(true);
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    let size = 30 * 1000 * 1000;
    fake_folder.remote_modifier().insert("A/a0", size, b'W');

    // First, download only the first 3 MB of the file
    fake_folder.set_server_override(|request, state| {
        if request.method() == http::Method::GET && path_ends_with(request, "A/a0") {
            return Some(broken_fake_get_reply(&state.remote_root, request));
        }
        None
    });

    assert!(!fake_folder.sync_once()); // The sync must fail because not all the file was downloaded
    assert_eq!(get_item(&complete_spy, "A/a0").status, Status::SoftError);
    assert_eq!(
        get_item(&complete_spy, "A/a0").error_string,
        "The file could not be downloaded completely."
    );
    assert_ne!(
        fake_folder.sync_engine().is_another_sync_needed(),
        nc_sync::AnotherSyncNeeded::NoFollowUpSync
    );

    // Now, we need to restart, this time, it should resume.
    let ranges = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let ranges = ranges.clone();
        fake_folder.set_server_override(move |request, _| {
            if request.method() == http::Method::GET && path_ends_with(request, "A/a0") {
                *ranges.lock().unwrap() = request
                    .headers()
                    .get("Range")
                    .map(|v| v.as_bytes().to_vec())
                    .unwrap_or_default();
            }
            None
        });
    }
    assert!(fake_folder.sync_once()); // now this succeeds
    assert_eq!(
        *ranges.lock().unwrap(),
        format!("bytes={STOP_AFTER}-").into_bytes()
    );
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_error_message() {
    // This test's main goal is to test that the error string from the server is shown in the UI

    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.sync_engine().set_ignore_hidden_files(true);
    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    let size = 3_500_000;
    fake_folder.remote_modifier().insert("A/broken", size, b'W');

    let server_message = "The file was not downloaded because the tests wants so!";

    // First, download only the first 3 MB of the file
    fake_folder.set_server_override(move |request, _| {
        if request.method() == http::Method::GET && path_ends_with(request, "A/broken") {
            return Some(
                error_reply(
                    400,
                    Bytes::from(format!(
                        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                         <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n\
                         <s:exception>Sabre\\DAV\\Exception\\Forbidden</s:exception>\n\
                         <s:message>{server_message}</s:message>\n\
                         </d:error>"
                    )),
                )
                .into(),
            );
        }
        None
    });

    let timer = fake_folder.abort_timer();
    timer.single_shot(Duration::from_millis(10000));
    assert!(!fake_folder.sync_once()); // Fail because A/broken
    let timed_out = timer.fired_count() > 0;
    assert!(!timed_out);
    assert_eq!(
        get_item(&complete_spy, "A/broken").status,
        Status::NormalError
    );
    assert!(
        get_item(&complete_spy, "A/broken")
            .error_string
            .contains(server_message)
    );
}

#[test]
fn server_maintenence() {
    // Server in maintenance must abort the sync.

    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.remote_modifier().insert("A/broken", 64, b'W');
    fake_folder.set_server_override(|request, _| {
        if request.method() == http::Method::GET {
            return Some(
                error_reply(
                    503,
                    Bytes::from_static(
                        b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                          <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n\
                          <s:exception>Sabre\\DAV\\Exception\\ServiceUnavailable</s:exception>\n\
                          <s:message>System in maintenance mode.</s:message>\n\
                          </d:error>",
                    ),
                )
                .into(),
            );
        }
        None
    });

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(!fake_folder.sync_once()); // Fail because A/broken
    // FatalError means the sync was aborted, which is what we want
    assert_eq!(
        get_item(&complete_spy, "A/broken").status,
        Status::FatalError
    );
    assert!(
        get_item(&complete_spy, "A/broken")
            .error_string
            .contains("System in maintenance mode")
    );
}

#[test]
fn test_move_fails_in_a_conflict() {
    // Test for https://github.com/owncloud/client/issues/7015
    // We want to test the case in which the renaming of the original to the conflict file succeeds,
    // but renaming the temporary file fails.
    // This tests uses the fact that a "touchedFile" notification will be sent at the right moment.
    // Note that there will be first a notification on the file and the conflict file before.

    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.sync_engine().set_ignore_hidden_files(true);
    fake_folder.remote_modifier().set_contents("A/a1", b'A');
    fake_folder.local_modifier().set_contents("A/a1", b'B');

    let prop_connected = Rc::new(Cell::new(false));
    let conflict_file = Rc::new(RefCell::new(String::new()));
    let local_path = fake_folder.local_path();
    {
        let prop_connected = prop_connected.clone();
        fake_folder.sync_engine().callbacks().transmission_progress = Some(Box::new(move |pi| {
            if pi.status() != ProgressStatus::Propagation || prop_connected.get() {
                return;
            }
            prop_connected.set(true);
        }));
    }
    {
        // connect(propagator, &OwncloudPropagator::touchedFile, ...) once
        // the propagation started.
        let prop_connected = prop_connected.clone();
        let conflict_file = conflict_file.clone();
        let local_path = local_path.clone();
        fake_folder.sync_engine().callbacks().propagator_event = Some(Box::new(move |ev| {
            let PropagatorEvent::TouchedFile(s) = ev else {
                return;
            };
            if !prop_connected.get() {
                return;
            }
            if s.contains("conflicted copy") {
                assert_eq!(*conflict_file.borrow(), "");
                *conflict_file.borrow_mut() = s.clone();
                return;
            }
            if !conflict_file.borrow().is_empty() {
                // Check that the temporary file is still there
                let dir = format!("{local_path}A/");
                let count = std::fs::read_dir(&dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter(|e| e.file_name().to_string_lossy().contains(".~"))
                    .count();
                assert_eq!(count, 1);
                // Set the permission to read only on the folder, so the rename of the temporary file will fail
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            }
        }));
    }

    assert!(!fake_folder.sync_once()); // The sync must fail because the rename failed
    assert!(!conflict_file.borrow().is_empty());

    // restore permissions
    std::fs::set_permissions(
        format!("{local_path}A/"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();

    // QObject::disconnect(transProgress) (and the old propagator is gone)
    fake_folder.sync_engine().callbacks().transmission_progress = None;
    fake_folder.sync_engine().callbacks().propagator_event = None;
    let downloaded = Arc::new(AtomicBool::new(false));
    {
        let downloaded = downloaded.clone();
        fake_folder.set_server_override(move |request, _| {
            if request.method() == http::Method::GET {
                // QTest::qFail("There shouldn't be any download", __FILE__, __LINE__);
                downloaded.store(true, Ordering::SeqCst);
            }
            None
        });
    }
    assert!(fake_folder.sync_once());
    assert!(
        !downloaded.load(Ordering::SeqCst),
        "There shouldn't be any download"
    );

    // The a1 file is still tere and have the right content
    assert!(fake_folder.current_remote_state().find("A/a1").is_some());
    assert_eq!(
        fake_folder
            .current_remote_state()
            .find("A/a1")
            .unwrap()
            .content_char,
        b'A'
    );

    std::fs::remove_file(&*conflict_file.borrow()).unwrap(); // So the comparison succeeds;
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn test_http2_resend() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder
        .remote_modifier()
        .insert("A/resendme", 300, b'W');

    let server_message = "An unexpected error occurred. Please try syncing again or contact your server administrator if the issue continues.";
    let resend_actual = Arc::new(AtomicI32::new(0));
    let resend_expected = Arc::new(AtomicI32::new(2));

    // First, download only the first 3 MB of the file
    {
        let resend_actual = resend_actual.clone();
        let resend_expected = resend_expected.clone();
        fake_folder.set_server_override(move |request, _| {
            if request.method() == http::Method::GET
                && path_ends_with(request, "A/resendme")
                && resend_actual.load(Ordering::SeqCst) < resend_expected.load(Ordering::SeqCst)
            {
                let mut error_reply = error_reply(400, Bytes::from_static(b"ignore this body"));
                // setError(QNetworkReply::ContentReSendError, serverMessage),
                // setAttribute(Http2WasUsedAttribute, true),
                // setAttribute(HttpStatusCodeAttribute, QVariant())
                error_reply.extensions_mut().insert(ReplyOverride {
                    error: NetworkError::ContentReSendError,
                    http_status: None,
                    http2_was_used: true,
                });
                resend_actual.fetch_add(1, Ordering::SeqCst);
                return Some(error_reply.into());
            }
            None
        });
    }

    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(resend_actual.load(Ordering::SeqCst), 2);

    fake_folder.remote_modifier().append_byte("A/resendme");
    resend_actual.store(0, Ordering::SeqCst);
    resend_expected.store(10, Ordering::SeqCst);

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(!fake_folder.sync_once());
    assert_eq!(resend_actual.load(Ordering::SeqCst), 4); // the 4th fails because it only resends 3 times
    assert_eq!(
        get_item(&complete_spy, "A/resendme").status,
        Status::NormalError
    );
    assert!(
        get_item(&complete_spy, "A/resendme")
            .error_string
            .contains(server_message)
    );
}
