// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testremotediscovery.cpp (nextcloud/desktop v34.0.5).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use nc_sync::Instruction;
use nc_testutils::server::{error_reply, propfind_payload, raw_propfind_reply};
use nc_testutils::{FakeFolder, FakeReply, FileInfo, FileModifier, ItemCompletedSpy};

/// `FakeBrokenXmlPropfindReply`: the normal PROPFIND payload, truncated.
fn fake_broken_xml_propfind_reply(root: &FileInfo, req: &nc_dav::Request) -> FakeReply {
    let mut payload = propfind_payload(root, req).expect("PROPFIND target exists");
    assert!(payload.len() > 50);
    // turncate the XML
    payload.truncate(payload.len() - 20);
    raw_propfind_reply(Bytes::from(payload)).into()
}

/// `MissingPermissionsPropfindReply`.
fn missing_permissions_propfind_reply(root: &FileInfo, req: &nc_dav::Request) -> FakeReply {
    let mut payload = propfind_payload(root, req).expect("PROPFIND target exists");
    // If the propfind contains a single file without permissions, this is a server error
    let to_remove = "<oc:permissions>GRDNVCKW</oc:permissions>";
    let half = payload.len() / 2;
    let pos = payload[half..].find(to_remove).map(|p| p + half);
    assert!(pos.is_some_and(|p| p > 0));
    let pos = pos.unwrap();
    payload.replace_range(pos..pos + to_remove.len(), "");
    raw_propfind_reply(Bytes::from(payload)).into()
}

// enum ErrorKind: lower code are corresponding to HTML error code
const INVALID_XML: u16 = 1000;
const TIMEOUT: u16 = 1001;

/// `QMap::operator[]`: the child, or an empty `FileInfo`.
fn child(state: &FileInfo, name: &str) -> FileInfo {
    state.children.get(name).cloned().unwrap_or_default()
}

/// Restores `AbstractNetworkJob::httpTimeout` (`QScopedValueRollback`).
struct HttpTimeoutRollback(u64);

impl Drop for HttpTimeoutRollback {
    fn drop(&mut self) {
        nc_dav::jobs::set_http_timeout(self.0);
    }
}

// Check what happens when there is an error.
#[test]
fn test_remote_discovery_error() {
    let item_error_message = "An unexpected error occurred. Please try syncing again or contact your server administrator if the issue continues.";
    // _data rows: (row name = errorKind, expectedErrorString, syncSucceeds)
    let rows: Vec<(u16, &str, bool)> = vec![
        (
            400,
            "We couldn’t process your request. Please try syncing again later. If this keeps happening, contact your server administrator for help.",
            false,
        ),
        (
            401,
            "You need to sign in to continue. If you have trouble with your credentials, please reach out to your server administrator.",
            false,
        ),
        (
            403,
            "You don’t have access to this resource. If you think this is a mistake, please contact your server administrator.",
            true,
        ),
        (
            404,
            "We couldn’t find what you were looking for. It might have been moved or deleted. If you need help, contact your server administrator.",
            true,
        ),
        (
            407,
            "It seems you are using a proxy that required authentication. Please check your proxy settings and credentials. If you need help, contact your server administrator.",
            true,
        ),
        (
            408,
            "The request is taking longer than usual. Please try syncing again. If it still doesn’t work, reach out to your server administrator.",
            true,
        ),
        (
            409,
            "Server files changed while you were working. Please try syncing again. Contact your server administrator if the issue persists.",
            true,
        ),
        (
            410,
            "This folder or file isn’t available anymore. If you need assistance, please contact your server administrator.",
            true,
        ),
        (
            412,
            "The request could not be completed because some required conditions were not met. Please try syncing again later. If you need assistance, please contact your server administrator.",
            true,
        ),
        (
            413,
            "The file is too big to upload. You might need to choose a smaller file or contact your server administrator for assistance.",
            true,
        ),
        (
            414,
            "The address used to make the request is too long for the server to handle. Please try shortening the information you’re sending or contact your server administrator for assistance.",
            true,
        ),
        (
            415,
            "This file type isn’t supported. Please contact your server administrator for assistance.",
            true,
        ),
        (
            422,
            "The server couldn’t process your request because some information was incorrect or incomplete. Please try syncing again later, or contact your server administrator for assistance.",
            true,
        ),
        (
            423,
            "The resource you are trying to access is currently locked and cannot be modified. Please try changing it later, or contact your server administrator for assistance.",
            true,
        ),
        (
            428,
            "This request could not be completed because it is missing some required conditions. Please try again later, or contact your server administrator for help.",
            true,
        ),
        (
            429,
            "You made too many requests. Please wait and try again. If you keep seeing this, your server administrator can help.",
            true,
        ),
        (
            500,
            "Something went wrong on the server. Please try syncing again later, or contact your server administrator if the issue persists.",
            true,
        ),
        (
            502,
            "We’re having trouble connecting to the server. Please try again soon. If the issue persists, your server administrator can help you.",
            true,
        ),
        (
            503,
            "The server is busy right now. Please try connecting again in a few minutes or contact your server administrator if it’s urgent.",
            true,
        ),
        (
            504,
            "It’s taking too long to connect to the server. Please try again later. If you need help, contact your server administrator.",
            true,
        ),
        (
            505,
            "The server does not support the version of the connection being used. Contact your server administrator for help.",
            true,
        ),
        (
            507,
            "The server does not have enough space to complete your request. Please check how much quota your user has by contacting your server administrator.",
            true,
        ),
        (
            511,
            "Your network needs extra authentication. Please check your connection. Contact your server administrator for help if the issue persists.",
            true,
        ),
        (
            513,
            "You don’t have permission to access this resource. If you believe this is an error, contact your server administrator to ask for assistance.",
            true,
        ),
        // 200 should be an error since propfind should return 207
        (200, item_error_message, false),
        (INVALID_XML, item_error_message, false),
        (
            TIMEOUT,
            "The server took too long to respond. Check your connection and try syncing again. If it still doesn’t work, reach out to your server administrator.",
            false,
        ),
    ];
    for (error_kind, expected_error_string, sync_succeeds) in rows {
        remote_discovery_error(error_kind, expected_error_string, sync_succeeds);
    }
}

fn remote_discovery_error(error_kind: u16, expected_error_string: &str, sync_succeeds: bool) {
    let row = format!("row {error_kind}");
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

    // Do Some change as well
    fake_folder.local_modifier().insert("A/z1", 64, b'W');
    fake_folder.local_modifier().insert("B/z1", 64, b'W');
    fake_folder.local_modifier().insert("C/z1", 64, b'W');
    fake_folder.remote_modifier().insert("A/z2", 64, b'W');
    fake_folder.remote_modifier().insert("B/z2", 64, b'W');
    fake_folder.remote_modifier().insert("C/z2", 64, b'W');

    let old_local_state = fake_folder.current_local_state();
    let old_remote_state = fake_folder.current_remote_state();

    let error_folder = Arc::new(Mutex::new("dav/files/admin/B".to_owned()));
    {
        let error_folder = error_folder.clone();
        fake_folder.set_server_override(move |req, state| {
            if req.method().as_str() == "PROPFIND"
                && req
                    .uri()
                    .path()
                    .ends_with(error_folder.lock().unwrap().as_str())
            {
                if error_kind == INVALID_XML {
                    return Some(fake_broken_xml_propfind_reply(&state.remote_root, req));
                } else if error_kind == TIMEOUT {
                    return Some(FakeReply::Hang);
                } else if error_kind < 1000 {
                    return Some(error_reply(error_kind, Bytes::new()).into());
                }
            }
            None
        });
    }

    // So the test that test timeout finishes fast
    let _set_http_timeout =
        HttpTimeoutRollback(nc_dav::jobs::set_http_timeout(if error_kind == TIMEOUT {
            1
        } else {
            10000
        }));

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    let error_spy = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let error_spy = error_spy.clone();
        fake_folder.sync_engine().callbacks().sync_error = Some(Box::new(move |msg, _| {
            error_spy.borrow_mut().push(msg.to_owned())
        }));
    }
    assert_eq!(fake_folder.sync_once(), sync_succeeds, "{row}");

    // The folder B should not have been sync'ed (and in particular not removed)
    assert_eq!(
        child(&old_local_state, "B"),
        child(&fake_folder.current_local_state(), "B"),
        "{row}"
    );
    assert_eq!(
        child(&old_remote_state, "B"),
        child(&fake_folder.current_remote_state(), "B"),
        "{row}"
    );
    if !sync_succeeds {
        assert_eq!(error_spy.borrow().len(), 1, "{row}");
        assert_eq!(error_spy.borrow()[0], expected_error_string, "{row}");
    } else {
        assert_eq!(
            complete_spy.find_item("B").instruction,
            Instruction::Ignore,
            "{row}"
        );
        assert!(
            complete_spy
                .find_item("B")
                .error_string
                .contains(expected_error_string),
            "{row}: {}",
            complete_spy.find_item("B").error_string
        );

        // The other folder should have been sync'ed as the sync just ignored the faulty dir
        assert_eq!(
            child(&fake_folder.current_remote_state(), "A"),
            child(&fake_folder.current_local_state(), "A"),
            "{row}"
        );
        assert_eq!(
            child(&fake_folder.current_remote_state(), "C"),
            child(&fake_folder.current_local_state(), "C"),
            "{row}"
        );
        assert_eq!(
            complete_spy.find_item("A/z1").instruction,
            Instruction::New,
            "{row}"
        );
    }

    //
    // Check the same discovery error on the sync root
    //
    *error_folder.lock().unwrap() = "dav/files/admin/".to_owned();
    error_spy.borrow_mut().clear();
    assert!(!fake_folder.sync_once(), "{row}");
    assert_eq!(error_spy.borrow().len(), 1, "{row}");
    assert_eq!(error_spy.borrow()[0], expected_error_string, "{row}");
}

#[test]
fn test_missing_data() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.remote_modifier().insert("good", 64, b'W');
    fake_folder.remote_modifier().insert("noetag", 64, b'W');
    fake_folder
        .remote_modifier()
        .find_mut("noetag")
        .unwrap()
        .etag
        .clear();
    fake_folder.remote_modifier().insert("nofileid", 64, b'W');
    fake_folder
        .remote_modifier()
        .find_mut("nofileid")
        .unwrap()
        .file_id
        .clear();
    fake_folder.remote_modifier().mkdir("nopermissions");
    fake_folder
        .remote_modifier()
        .insert("nopermissions/A", 64, b'W');

    fake_folder.set_server_override(|req, state| {
        if req.method().as_str() == "PROPFIND" && req.uri().path().ends_with("nopermissions") {
            return Some(missing_permissions_propfind_reply(&state.remote_root, req));
        }
        None
    });

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(!fake_folder.sync_once());

    assert_eq!(complete_spy.find_item("good").instruction, Instruction::New);
    assert_eq!(
        complete_spy.find_item("noetag").instruction,
        Instruction::Error
    );
    assert_eq!(
        complete_spy.find_item("nofileid").instruction,
        Instruction::Error
    );
    assert_eq!(
        complete_spy.find_item("nopermissions").instruction,
        Instruction::New
    );
    assert_eq!(
        complete_spy.find_item("nopermissions/A").instruction,
        Instruction::Error
    );
    assert!(
        complete_spy
            .find_item("noetag")
            .error_string
            .contains("File is not accessible on the server")
    );
    assert!(
        complete_spy
            .find_item("nofileid")
            .error_string
            .contains("File is not accessible on the server")
    );
    assert!(
        complete_spy
            .find_item("nopermissions/A")
            .error_string
            .contains("File is not accessible on the server")
    );
}

#[test]
fn test_quota_reported_as_double() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    {
        let mut remote = fake_folder.remote_modifier();
        remote.mkdir("doubleValue");
        remote
            .find_mut("doubleValue")
            .unwrap()
            .folder_quota
            .set_bytes_available_string("2.345E+12");
        remote.mkdir("intValue");
        remote
            .find_mut("intValue")
            .unwrap()
            .folder_quota
            .set_bytes_available_string("2345000000000");
        remote.mkdir("unlimited");
        remote
            .find_mut("unlimited")
            .unwrap()
            .folder_quota
            .set_bytes_available_string("-3");
        remote.mkdir("invalidValue");
        remote
            .find_mut("invalidValue")
            .unwrap()
            .folder_quota
            .set_bytes_available_string("maybe like, 3 GB");
    }

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);
    assert!(fake_folder.sync_once());

    let expected_value: i64 = 2345000000000;
    assert_eq!(
        complete_spy
            .find_item("doubleValue")
            .folder_quota
            .bytes_available,
        expected_value
    );
    assert_eq!(
        complete_spy
            .find_item("intValue")
            .folder_quota
            .bytes_available,
        expected_value
    );
    assert_eq!(
        complete_spy
            .find_item("unlimited")
            .folder_quota
            .bytes_available,
        -3
    );
    assert_eq!(
        complete_spy
            .find_item("invalidValue")
            .folder_quota
            .bytes_available,
        -1
    );
}

#[test]
fn test_root_file_id_received() {
    // disable FakeFolder's initial sync run to allow for spying on the `rootFileIdReceived` signal
    let mut fake_folder = FakeFolder::with_options(FileInfo::A12_B12_C12_S12(), None, "", false);

    let root_file_id_spy = Rc::new(RefCell::new(Vec::<i64>::new()));
    {
        let spy = root_file_id_spy.clone();
        fake_folder.sync_engine().callbacks().root_file_id_received =
            Some(Box::new(move |id| spy.borrow_mut().push(id)));
    }
    assert!(fake_folder.sync_once());

    // root file id signal should only be emitted once
    assert_eq!(root_file_id_spy.borrow().len(), 1);
    let expected_root_file_id = fake_folder.current_remote_state().numeric_file_id();
    let received_root_file_id = root_file_id_spy.borrow_mut().remove(0).to_string();
    assert_eq!(received_root_file_id, expected_root_file_id);

    // another sync should not emit the root file id signal again as it's already known
    root_file_id_spy.borrow_mut().clear();
    assert!(fake_folder.sync_once());
    assert_eq!(root_file_id_spy.borrow().len(), 0);
}

// Regression: processEvents() inside LsColJob::finished() must not run before reply()
// is read. A pending deleteLater() draining during processEvents() zeros the QPointer
// and caused a SIGSEGV at reply()->request().url().path() (address 0x8).
//
// Adapted: there is no Qt object lifetime in the port (the reply is owned by
// the job until it is parsed), so FakePropfindReplyWithPendingDeletion is a
// plain FakePropfindReply here.
#[test]
fn test_ls_col_job_does_not_crash_when_reply_is_deleted_during_process_events() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.remote_modifier().mkdir("B/sub");

    let error_folder = "dav/files/admin/B";
    fake_folder.set_server_override(move |req, state| {
        if req.method().as_str() == "PROPFIND" && req.uri().path().ends_with(error_folder) {
            let payload = propfind_payload(&state.remote_root, req)?;
            return Some(raw_propfind_reply(Bytes::from(payload)).into());
        }
        None
    });

    // The sync must complete without crashing. Discovery of B will fail gracefully
    // because finishedWithoutError is still emitted before processEvents() runs.
    assert!(fake_folder.sync_once());
    // Items under A and C must have synced normally.
    assert!(fake_folder.current_local_state().find("A").is_some());
    assert!(fake_folder.current_local_state().find("C").is_some());
}

// Verifies normal listing succeeds when the reply stays alive throughout.
#[test]
fn test_ls_col_job_succeeds_normally() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder
        .remote_modifier()
        .insert("A/newfile.txt", 64, b'W');

    // Normal sync with no overrides: all PROPFIND replies remain alive.
    assert!(fake_folder.sync_once());
    assert!(
        fake_folder
            .current_local_state()
            .find("A/newfile.txt")
            .is_some()
    );
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}
