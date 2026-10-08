// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `test/testclientstatusreporting.cpp` (nextcloud/desktop
// v34.0.5).

//! Client status reporting: the statuses reported by an account whose
//! server has `security_guard.diagnostics` are counted in the reporting
//! database and sent with a PUT to the diagnostics endpoint.
//!
//! Upstream's functions share one account and one database; here every
//! test builds the fixture of `initTestCase` and tears it down like
//! `cleanupTestCase`. The send intervals are shortened like upstream
//! (1 s timer, 2 s between reports).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nc_dav::client_status::ClientStatusReportingStatus as Status;
use nc_dav::{Account, Capabilities, ServerUrl};
use nc_sync::client_status_reporting::{
    CLIENT_STATUS_REPORTING_TRY_SEND_TIMER_INTERVAL, ClientStatusReporting,
    REPORT_SEND_INTERVAL_MS, make_db_path,
};
use nc_testutils::server::json_reply;
use nc_testutils::{FakeReply, FakeServer, FileInfo};
use serde_json::{Value, json};

const FAKE_200_RESPONSE: &str =
    r#"{"ocs":{"meta":{"status":"success","statuscode":200},"data":[]}}"#;

const TRY_SEND_TIMER_INTERVAL: u64 = 1000;
const REPORT_SEND_INTERVAL: u64 = 2000;

/// The members of `TestClientStatusReporting`.
struct Fixture {
    account: Arc<Account>,
    /// `bodyReceivedAndParsed`.
    body_received_and_parsed: Arc<Mutex<serde_json::Map<String, Value>>>,
    db_dir: tempfile::TempDir,
}

/// `initTestCase()`; must run inside the tokio runtime (the reporting's
/// timer is started when the capabilities are set).
fn init_test_case() -> Fixture {
    CLIENT_STATUS_REPORTING_TRY_SEND_TIMER_INTERVAL
        .store(TRY_SEND_TIMER_INTERVAL, Ordering::SeqCst);
    REPORT_SEND_INTERVAL_MS.store(REPORT_SEND_INTERVAL, Ordering::SeqCst);

    let server = FakeServer::new(FileInfo::default());
    let body_received_and_parsed = Arc::new(Mutex::new(serde_json::Map::new()));
    {
        let body = body_received_and_parsed.clone();
        server.set_override(move |request, _| {
            let req_body = request
                .body()
                .as_bytes()
                .map(|b| b.to_vec())
                .unwrap_or_default();
            *body.lock().unwrap() = serde_json::from_slice::<Value>(&req_body)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            Some(FakeReply::Response(json_reply(200, FAKE_200_RESPONSE)))
        });
    }
    let account = Arc::new(Account::new(
        ServerUrl::parse("http://example.de").unwrap(),
        Arc::new(server),
    ));

    // dbPathForTesting: a temporary directory.
    let db_dir = tempfile::tempdir().unwrap();
    let db_file_path = make_db_path(db_dir.path(), &account);
    let _ = std::fs::remove_file(&db_file_path);
    account.enable_client_status_reporting(Arc::new(move |account| {
        ClientStatusReporting::new(account, &db_file_path)
            .map(|r| Arc::new(r) as Arc<dyn nc_dav::client_status::ClientStatusReporter>)
    }));

    account.set_capabilities(Capabilities::from_json(json!({
        "security_guard": { "diagnostics": true }
    })));
    Fixture {
        account,
        body_received_and_parsed,
        db_dir,
    }
}

/// `cleanupTestCase()`.
fn cleanup_test_case(fixture: Fixture) {
    drop(fixture.account);
    drop(fixture.db_dir);
}

/// `QTest::qWait(trySendTimerInterval + reportSendInterval)`.
async fn wait_for_send() {
    tokio::time::sleep(Duration::from_millis(
        TRY_SEND_TIMER_INTERVAL + REPORT_SEND_INTERVAL,
    ))
    .await;
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn test_report_and_send_statuses() {
    runtime().block_on(async {
        let f = init_test_case();
        let account = &f.account;
        for _ in 0..2 {
            // 2 conflicts
            account.report_client_status(Status::DownloadErrorConflictInvalidCharacters);
            account.report_client_status(Status::DownloadErrorConflictCaseClash);

            // 3 problems
            account.report_client_status(Status::UploadErrorServerError);
            account.report_client_status(Status::DownloadErrorServerError);
            account.report_client_status(Status::DownloadErrorVirtualFileHydrationFailure);
            // 3 occurances of case ClientStatusReportingStatus::UploadError_No_Write_Permissions
            account.report_client_status(Status::DownloadErrorVirtualFileHydrationFailure);
            account.report_client_status(Status::DownloadErrorVirtualFileHydrationFailure);

            // 2 occurances of E2EeError_GeneralError
            account.report_client_status(Status::E2EeErrorGeneralError);
            account.report_client_status(Status::E2EeErrorGeneralError);

            // 3 occurances of case ClientStatusReportingStatus::UploadError_Virus_Detected
            account.report_client_status(Status::UploadErrorVirusDetected);
            account.report_client_status(Status::UploadErrorVirusDetected);
            account.report_client_status(Status::UploadErrorVirusDetected);

            wait_for_send().await;

            let body = f.body_received_and_parsed.lock().unwrap().clone();
            assert!(!body.is_empty());

            // we must have 3 virus_detected category statuses
            let virus_detected_errors_received = body["virus_detected"].as_object().unwrap();
            assert!(!virus_detected_errors_received.is_empty());
            assert_eq!(virus_detected_errors_received["count"], 3);

            // we must have 2 e2ee errors
            let e2ee_errors_received = body["e2ee_errors"].as_object().unwrap();
            assert!(!e2ee_errors_received.is_empty());
            assert_eq!(e2ee_errors_received["count"], 2);

            // we must have 2 conflicts
            let conflicts_received = body["sync_conflicts"].as_object().unwrap();
            assert!(!conflicts_received.is_empty());
            assert_eq!(conflicts_received["count"], 2);

            // we must have 3 problems
            let problems_received = body["problems"].as_object().unwrap();
            assert!(!problems_received.is_empty());
            assert_eq!(problems_received.len(), 3);
            let specific_problem_multiple_occurances = problems_received
                [Status::DownloadErrorVirtualFileHydrationFailure.status_string()]
            .as_object()
            .unwrap();
            // among those, 3 occurances of specific problem
            assert_eq!(specific_problem_multiple_occurances["count"], 3);

            f.body_received_and_parsed.lock().unwrap().clear();
        }
        cleanup_test_case(f);
    });
}

#[test]
fn test_nothing_reported_and_nothing_sent() {
    runtime().block_on(async {
        let f = init_test_case();
        wait_for_send().await;
        assert!(f.body_received_and_parsed.lock().unwrap().is_empty());
        cleanup_test_case(f);
    });
}

/// rust_only: without the capability nothing is set up, and a capability
/// switched off drops the reporting (`trySetupClientStatusReporting`).
#[test]
fn rust_only_reporting_follows_the_capability() {
    runtime().block_on(async {
        let f = init_test_case();
        let db = make_db_path(f.db_dir.path(), &f.account);
        assert!(db.exists(), "the database is created with the reporting");
        f.account
            .set_capabilities(Capabilities::from_json(json!({ "security_guard": {} })));
        f.account
            .report_client_status(Status::UploadErrorServerError);
        wait_for_send().await;
        assert!(f.body_received_and_parsed.lock().unwrap().is_empty());
        cleanup_test_case(f);
    });
}

/// rust_only: the sync engine reports the failures of its items
/// (`PropagateItemJob::reportClientStatuses`): a failed upload is an
/// `UploadError.SERVER_ERROR`, a failed download a
/// `DownloadError.SERVER_ERROR`.
#[test]
fn rust_only_sync_engine_reports_item_errors() {
    use nc_sync::client_status_reporting::ClientStatusReportingDatabase;
    use nc_testutils::{FakeFolder, FileModifier};

    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    let db_dir = tempfile::tempdir().unwrap();
    let db_file_path = make_db_path(db_dir.path(), fake_folder.account());
    {
        let db_file_path = db_file_path.clone();
        fake_folder
            .account()
            .enable_client_status_reporting(Arc::new(move |account| {
                ClientStatusReporting::new(account, &db_file_path)
                    .map(|r| Arc::new(r) as Arc<dyn nc_dav::client_status::ClientStatusReporter>)
            }));
    }
    let mut caps = fake_folder.account().capabilities().raw().clone();
    caps.insert("security_guard".to_owned(), json!({ "diagnostics": true }));
    fake_folder
        .account()
        .set_capabilities(Capabilities::new(caps));

    fake_folder.local_modifier().insert("A/up", 10, b'u');
    fake_folder.remote_modifier().insert("B/down", 10, b'd');
    fake_folder.server_error_paths().append("A/up", 500);
    fake_folder.server_error_paths().append("B/down", 500);
    assert!(!fake_folder.sync_once());

    let db = ClientStatusReportingDatabase::open(&db_file_path).unwrap();
    let mut names: Vec<(String, i64)> = db
        .get_client_status_reporting_records()
        .into_iter()
        .map(|r| (String::from_utf8(r.name).unwrap(), r.num_occurences))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("DownloadError.SERVER_ERROR".to_owned(), 1),
            ("UploadError.SERVER_ERROR".to_owned(), 1)
        ]
    );
}
