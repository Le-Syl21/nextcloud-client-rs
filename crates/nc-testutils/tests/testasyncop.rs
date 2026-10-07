/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testasyncop.cpp (nextcloud/desktop v34.0.5).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{Method, StatusCode};
use nc_dav::{Body, Request};
use nc_testutils::server::{
    chunk_move_perform, file_path_from_url, payload_reply, put_perform, status_reply,
};
use nc_testutils::{AbortTrigger, FakeFolder, FakeReply, FileInfo, FileModifier, ServerState};

/// `FakeAsyncReply`: answers 202 with `OC-JobStatus-Location: <pollLocation>`.
fn fake_async_reply(poll_location: &str) -> FakeReply {
    let mut resp = status_reply(StatusCode::ACCEPTED);
    resp.headers_mut()
        .insert("OC-JobStatus-Location", poll_location.parse().unwrap());
    resp.into()
}

/// A copy of a request (the payload of the fake server's requests is
/// always in memory), kept by the `perform` closures like upstream's
/// lambdas capture the `QNetworkRequest` and the payload.
fn clone_request(request: &Request) -> Request {
    let body = request.body().as_bytes().cloned().unwrap_or_default();
    let mut copy = http::Request::new(Body::Full(body));
    *copy.method_mut() = request.method().clone();
    *copy.uri_mut() = request.uri().clone();
    *copy.headers_mut() = request.headers().clone();
    copy
}

/// The decoded path of the request URL (`request.url().path()`).
fn url_path(request: &Request) -> String {
    percent_encoding::percent_decode_str(request.uri().path())
        .decode_utf8_lossy()
        .into_owned()
}

// This test is made of several testcases.
// the testCases maps a filename to a couple of callback.
// When a file is uploaded, the fake server will always return the 202 code, and will set
// the `perform` functor to what needs to be done to complete the transaction.
// The testcase consist of the `pollRequest` which will be called when the sync engine
// calls the poll url.
type PollRequest =
    Arc<dyn Fn(&mut TestCase, &Request, &mut ServerState) -> FakeReply + Send + Sync>;
/// Completes the transaction on the server; returns the etag and file id of
/// the resulting file (upstream returns the `FileInfo *`).
type Perform = Box<dyn FnOnce(&mut ServerState) -> (String, String) + Send>;

struct TestCase {
    poll_request: PollRequest,
    perform: Option<Perform>,
}

impl TestCase {
    fn new(poll_request: PollRequest) -> Self {
        Self {
            poll_request,
            perform: None,
        }
    }
}

type TestCases = Arc<Mutex<HashMap<String, TestCase>>>;

/// Answers a GET of `/async-poll/<file>` with the test case's `pollRequest`.
fn handle_poll(
    test_cases: &TestCases,
    request: &Request,
    state: &mut ServerState,
) -> Option<FakeReply> {
    let path = url_path(request);
    if request.method() == Method::GET
        && let Some(file) = path.strip_prefix("/async-poll/")
    {
        let mut cases = test_cases.lock().unwrap();
        let test_case = cases.get_mut(file).expect("testCases.contains(file)");
        let poll_request = test_case.poll_request.clone();
        return Some(poll_request(test_case, request, state));
    }
    None
}

fn shall_no_longer_be_called() -> PollRequest {
    Arc::new(|_, _, _| std::process::abort())
}

#[test]
fn async_upload_operations() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.set_capabilities(serde_json::json!({ "dav": { "chunking": "1.0" } }));
    // Reduce max chunk size a bit so we get more chunks
    let mut options = nc_sync::SyncOptions::default();
    options.set_max_chunk_size(20 * 1000);
    // `SyncEngine::minimumFileAgeForUpload` is a static that FakeFolder sets to 0.
    options.minimum_file_age_for_upload = std::time::Duration::ZERO;
    let max_chunk_size = options.max_chunk_size();
    fake_folder.sync_engine().set_sync_options(options);
    let n_get = Arc::new(AtomicUsize::new(0));

    let test_cases: TestCases = Arc::default();

    {
        let test_cases = test_cases.clone();
        let n_get = n_get.clone();
        fake_folder.set_server_override(move |request, state| {
            if let Some(reply) = handle_poll(&test_cases, request, state) {
                return Some(reply);
            }
            let path = url_path(request);

            if request.method() == Method::PUT && !path.contains("/uploads/") {
                // Not chunking
                let file = file_path_from_url(request.uri()).unwrap_or_default();
                let mut cases = test_cases.lock().unwrap();
                let test_case = cases.get_mut(&file).expect("testCases.contains(file)");
                assert!(test_case.perform.is_none());
                let put_request = clone_request(request);
                let put_file = file.clone();
                test_case.perform = Some(Box::new(move |state: &mut ServerState| {
                    let info =
                        put_perform(&mut state.remote_root, &put_request, &put_file).unwrap();
                    (info.etag.clone(), info.file_id.clone())
                }));
                return Some(fake_async_reply(&format!("/async-poll/{file}")));
            } else if request.method().as_str() == "MOVE" {
                let destination = request
                    .headers()
                    .get("Destination")
                    .and_then(|d| d.to_str().ok())
                    .and_then(|d| d.parse().ok())
                    .unwrap();
                let file = file_path_from_url(&destination).unwrap_or_default();
                let mut cases = test_cases.lock().unwrap();
                let test_case = cases.get_mut(&file).expect("testCases.contains(file)");
                assert!(test_case.perform.is_none());
                let move_request = clone_request(request);
                test_case.perform = Some(Box::new(move |state: &mut ServerState| {
                    let source = file_path_from_url(move_request.uri()).unwrap();
                    let ServerState {
                        remote_root,
                        upload_root,
                        ..
                    } = state;
                    let info = chunk_move_perform(upload_root, remote_root, &move_request, &source)
                        .unwrap();
                    (info.etag.clone(), info.file_id.clone())
                }));
                return Some(fake_async_reply(&format!("/async-poll/{file}")));
            } else if request.method() == Method::GET {
                n_get.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    }

    // Callback to be used to finalize the transaction and return the success
    let success_callback: PollRequest = Arc::new(|tc, _request, state| {
        tc.poll_request = shall_no_longer_be_called(); // shall no longer be called
        let (etag, file_id) = (tc.perform.take().expect("perform"))(state);
        let body = format!(
            "{{ \"status\":\"finished\", \"ETag\":\"\\\"{etag}\\\"\", \"fileId\":\"{file_id}\"}}\n"
        );
        payload_reply(Bytes::from(body)).into()
    });
    // Callback that never finishes
    let wait_forever_callback: PollRequest = Arc::new(|_, _request, _| {
        let body = "{\"status\":\"started\"}\n";
        payload_reply(Bytes::from_static(body.as_bytes())).into()
    });
    // Callback that simulate an error.
    let error_callback: PollRequest = Arc::new(|tc, _request, _| {
        tc.poll_request = shall_no_longer_be_called(); // shall no longer be called;
        let body = "{\"status\":\"error\",\"errorCode\":500,\"errorMessage\":\"TestingErrors\"}\n";
        payload_reply(Bytes::from_static(body.as_bytes())).into()
    });
    // This lambda takes another functor as a parameter, and returns a callback that will
    // tell the client needs to poll again, and further call to the poll url will call the
    // given callback
    fn wait_and_chain(chain: PollRequest) -> PollRequest {
        Arc::new(move |tc, _request, _| {
            tc.poll_request = chain.clone();
            let body = "{\"status\":\"started\"}\n";
            payload_reply(Bytes::from_static(body.as_bytes())).into()
        })
    }

    // Create a testcase by creating a file of a given size locally and assigning it a callback
    let insert_file = |fake_folder: &mut FakeFolder, file: &str, size: i64, cb: PollRequest| {
        fake_folder.local_modifier().insert(file, size, b'W');
        test_cases
            .lock()
            .unwrap()
            .insert(file.to_owned(), TestCase::new(cb));
    };
    fake_folder.local_modifier().mkdir("success");
    insert_file(
        &mut fake_folder,
        "success/chunked_success",
        max_chunk_size * 3,
        success_callback.clone(),
    );
    insert_file(
        &mut fake_folder,
        "success/single_success",
        300,
        success_callback.clone(),
    );
    insert_file(
        &mut fake_folder,
        "success/chunked_patience",
        max_chunk_size * 3,
        wait_and_chain(wait_and_chain(success_callback.clone())),
    );
    insert_file(
        &mut fake_folder,
        "success/single_patience",
        300,
        wait_and_chain(wait_and_chain(success_callback.clone())),
    );
    fake_folder.local_modifier().mkdir("err");
    insert_file(
        &mut fake_folder,
        "err/chunked_error",
        max_chunk_size * 3,
        error_callback.clone(),
    );
    insert_file(
        &mut fake_folder,
        "err/single_error",
        300,
        error_callback.clone(),
    );
    insert_file(
        &mut fake_folder,
        "err/chunked_error2",
        max_chunk_size * 3,
        wait_and_chain(error_callback.clone()),
    );
    insert_file(
        &mut fake_folder,
        "err/single_error2",
        300,
        wait_and_chain(error_callback.clone()),
    );

    // First sync should finish by itself.
    // All the things in "success/" should be transferred, the things in "err/" not
    assert!(!fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 0);
    // Upstream dereferences both `find()` results.
    assert_eq!(
        fake_folder.current_local_state().find("success").unwrap(),
        fake_folder.current_remote_state().find("success").unwrap()
    );
    {
        let mut cases = test_cases.lock().unwrap();
        cases.clear();
        cases.insert(
            "err/chunked_error".to_owned(),
            TestCase::new(success_callback.clone()),
        );
        cases.insert(
            "err/chunked_error2".to_owned(),
            TestCase::new(success_callback.clone()),
        );
        cases.insert(
            "err/single_error".to_owned(),
            TestCase::new(success_callback.clone()),
        );
        cases.insert(
            "err/single_error2".to_owned(),
            TestCase::new(success_callback.clone()),
        );
    }

    let abort_trigger = AbortTrigger::default();
    fake_folder.local_modifier().mkdir("waiting");
    insert_file(
        &mut fake_folder,
        "waiting/small",
        300,
        wait_forever_callback.clone(),
    );
    insert_file(
        &mut fake_folder,
        "waiting/willNotConflict",
        300,
        wait_forever_callback.clone(),
    );
    {
        let abort_trigger = abort_trigger.clone();
        let wait_forever_callback = wait_forever_callback.clone();
        insert_file(
            &mut fake_folder,
            "waiting/big",
            max_chunk_size * 3,
            wait_and_chain(wait_and_chain(Arc::new(move |tc, request, state| {
                // QTimer::singleShot(0, &fakeFolder.syncEngine(), &SyncEngine::abort);
                abort_trigger.abort();
                wait_and_chain(wait_forever_callback.clone())(tc, request, state)
            }))),
        );
    }

    assert!(fake_folder.sync_journal().wipe_error_blacklist() != -1);

    // This second sync will redo the files that had errors
    // But the waiting folder will not complete before it is aborted.
    assert!(!fake_folder.sync_once_abortable(&abort_trigger));
    assert_eq!(n_get.load(Ordering::SeqCst), 0);
    // Upstream dereferences both `find()` results.
    assert_eq!(
        fake_folder.current_local_state().find("err").unwrap(),
        fake_folder.current_remote_state().find("err").unwrap()
    );

    {
        let mut cases = test_cases.lock().unwrap();
        cases.get_mut("waiting/small").unwrap().poll_request =
            wait_and_chain(wait_and_chain(success_callback.clone()));
        cases.get_mut("waiting/big").unwrap().poll_request =
            wait_and_chain(success_callback.clone());
        let success_callback = success_callback.clone();
        cases
            .get_mut("waiting/willNotConflict")
            .unwrap()
            .poll_request = Arc::new(move |tc, request, state| {
            let reply = success_callback(tc, request, state);
            // This is going to succeed, and after we just change the file.
            // This should not be a conflict, but this should be downloaded in the
            // next sync
            state.remote_root.append_byte("waiting/willNotConflict");
            reply
        });
    }

    let n_put = Arc::new(AtomicUsize::new(0));
    let n_move = Arc::new(AtomicUsize::new(0));
    let n_delete = Arc::new(AtomicUsize::new(0));
    {
        let test_cases = test_cases.clone();
        let (n_get, n_put, n_move, n_delete) = (
            n_get.clone(),
            n_put.clone(),
            n_move.clone(),
            n_delete.clone(),
        );
        fake_folder.set_server_override(move |request, state| {
            if let Some(reply) = handle_poll(&test_cases, request, state) {
                return Some(reply);
            } else if request.method() == Method::PUT {
                n_put.fetch_add(1, Ordering::SeqCst);
            } else if request.method() == Method::GET {
                n_get.fetch_add(1, Ordering::SeqCst);
            } else if request.method() == Method::DELETE {
                n_delete.fetch_add(1, Ordering::SeqCst);
            } else if request.method().as_str() == "MOVE" {
                n_move.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    }

    // This last sync will do the waiting stuff
    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 1); // "waiting/willNotConflict"
    assert_eq!(n_put.load(Ordering::SeqCst), 0);
    assert_eq!(n_move.load(Ordering::SeqCst), 0);
    assert_eq!(n_delete.load(Ordering::SeqCst), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}
