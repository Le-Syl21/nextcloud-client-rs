// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/clientstatusreporting.cpp`,
// `clientstatusreportingdatabase.cpp`, `clientstatusreportingnetwork.cpp`
// and `clientstatusreportingrecord.h` (nextcloud/desktop v34.0.5).

//! Client status reporting (`security_guard` diagnostics): when the
//! server's capability `security_guard.diagnostics` is on, the account
//! records some sync failures (conflicts, server errors, virus detected,
//! end-to-end encryption errors) in a small SQLite database
//! (`<config dir>/.userdata_<hash>.db`), and sends their counts to
//! `ocs/v2.php/apps/security_guard/diagnostics` at most once every 24
//! hours (checked every 2 minutes).
//!
//! [`install`] gives an account the factory that creates its reporting
//! when the capability is on (`Account::trySetupClientStatusReporting`).
//!
//! Upstream quirk not reproduced: every `ClientStatusReportingDatabase`
//! opens Qt's default SQL connection, so with several accounts all of them
//! write to the last opened database; here each account has its own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use md5::{Digest, Md5};
use nc_dav::Account;
use nc_dav::client_status::{ClientStatusReporter, ClientStatusReportingStatus};
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

const LOG: &str = "nextcloud.sync.clientstatusreporting";
const LOG_DB: &str = "nextcloud.sync.clientstatusreportingdatabase";
const LOG_NET: &str = "nextcloud.sync.clientstatusreportingnetwork";

const LAST_SENT_REPORT_TIMESTAMP: &str = "lastClientStatusReportSentTime";
const STATUS_NAMES_HASH: &str = "statusNamesHash";

const STATUS_REPORT_CATEGORY_E2E_ERRORS: &str = "e2ee_errors";
const STATUS_REPORT_CATEGORY_PROBLEMS: &str = "problems";
const STATUS_REPORT_CATEGORY_SYNC_CONFLICTS: &str = "sync_conflicts";
const STATUS_REPORT_CATEGORY_VIRUS: &str = "virus_detected";

/// `ClientStatusReportingNetwork::clientStatusReportingTrySendTimerInterval`:
/// check if the time has come, every 2 minutes (milliseconds; tests change it).
pub static CLIENT_STATUS_REPORTING_TRY_SEND_TIMER_INTERVAL: AtomicU64 =
    AtomicU64::new(2 * 60 * 1000);
/// `ClientStatusReportingNetwork::repordSendIntervalMs`: once every 24 hours.
pub static REPORT_SEND_INTERVAL_MS: AtomicU64 = AtomicU64::new(24 * 60 * 60 * 1000);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `ClientStatusReportingRecord`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientStatusReportingRecord {
    pub name: Vec<u8>,
    pub status: i64,
    pub num_occurences: i64,
    pub last_occurence: i64,
}

/// `ClientStatusReportingDatabase::makeDbPath(account)`:
/// `<config dir>/.userdata_<first 6 bytes of md5("davUser@url") in hex>.db`.
pub fn make_db_path(config_dir: &Path, account: &Account) -> PathBuf {
    let database_id = format!(
        "{}@{}",
        account.dav_user(),
        account.url().to_credential_free_string()
    );
    let hash = Md5::digest(database_id.as_bytes());
    let hex: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
    config_dir.join(format!(".userdata_{hex}.db"))
}

/// `ClientStatusReportingDatabase`.
pub struct ClientStatusReportingDatabase {
    db: Mutex<Connection>,
}

impl ClientStatusReportingDatabase {
    /// Opens (creates) the database and its tables; `None` when that fails
    /// (`isInitialized()` false).
    pub fn open(path: &Path) -> Option<Self> {
        let conn = match Connection::open(path) {
            Ok(c) => c,
            Err(_) => {
                log::warn!(target: LOG_DB, "Could not setup client reporting, database connection error.");
                return None;
            }
        };
        if let Err(e) = conn.execute(
            "CREATE TABLE IF NOT EXISTS clientstatusreporting(name VARCHAR(4096) PRIMARY KEY,status INTEGER(8),count INTEGER,lastOccurrence INTEGER(8))",
            [],
        ) {
            log::warn!(target: LOG_DB, "Could not setup client clientstatusreporting table: {e}");
            return None;
        }
        if let Err(e) = conn.execute(
            "CREATE TABLE IF NOT EXISTS keyvalue(key VARCHAR(4096), value VARCHAR(4096), PRIMARY KEY(key))",
            [],
        ) {
            log::warn!(target: LOG_DB, "Could not setup client keyvalue table: {e}");
            return None;
        }
        let db = Self {
            db: Mutex::new(conn),
        };
        if !db.update_status_names_hash() {
            return None;
        }
        Some(db)
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `getClientStatusReportingRecords()`.
    pub fn get_client_status_reporting_records(&self) -> Vec<ClientStatusReportingRecord> {
        let conn = self.conn();
        let mut stmt = match conn.prepare("SELECT * FROM clientstatusreporting") {
            Ok(s) => s,
            Err(e) => {
                log::warn!(target: LOG_DB, "Could not get records from clientstatusreporting: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok(ClientStatusReportingRecord {
                name: value_bytes(row.get_ref("name")?),
                status: row.get::<_, Option<i64>>("status")?.unwrap_or(0),
                num_occurences: row.get::<_, Option<i64>>("count")?.unwrap_or(0),
                last_occurence: row.get::<_, Option<i64>>("lastOccurrence")?.unwrap_or(0),
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                log::warn!(target: LOG_DB, "Could not get records from clientstatusreporting: {e}");
                Vec::new()
            }
        }
    }

    /// `deleteClientStatusReportingRecords()`.
    pub fn delete_client_status_reporting_records(&self) -> Result<(), String> {
        self.conn()
            .execute("DELETE FROM clientstatusreporting", [])
            .map(|_| ())
            .map_err(|e| {
                log::warn!(target: LOG_DB, "Could not delete records from clientstatusreporting: {e}");
                e.to_string()
            })
    }

    /// `setClientStatusReportingRecord(record)`: a new record with count 1,
    /// or one more occurrence of an existing one.
    pub fn set_client_status_reporting_record(
        &self,
        record: &ClientStatusReportingRecord,
    ) -> Result<(), String> {
        if record.name.is_empty() {
            log::warn!(target: LOG_DB, "Failed to set ClientStatusReportingRecord");
            return Err("Invalid parameter".to_owned());
        }
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO clientstatusreporting (name, status, count, lastOccurrence) VALUES(?1, ?2, ?3, ?4) ON CONFLICT(name) DO UPDATE SET count = count + 1, lastOccurrence = ?4;",
                // QByteArray values are bound as blobs by Qt's SQLite driver.
                params![record.name, record.status, 1, record.last_occurence],
            )
            .map(|_| ())
            .map_err(|e| {
                log::warn!(target: LOG_DB, "Could not report client status: {e}");
                e.to_string()
            })
    }

    fn get_key_value(&self, key: &str) -> Result<Option<rusqlite::types::Value>, rusqlite::Error> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT value FROM keyvalue WHERE key = (?1)")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    fn set_key_value(&self, key: &str, value: rusqlite::types::Value) -> Result<(), String> {
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO keyvalue (key, value) VALUES(?1, ?2);",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// `getLastSentReportTimestamp()` (0 when never sent).
    pub fn get_last_sent_report_timestamp(&self) -> u64 {
        match self.get_key_value(LAST_SENT_REPORT_TIMESTAMP) {
            Ok(Some(v)) => match v {
                rusqlite::types::Value::Integer(i) => i.max(0) as u64,
                rusqlite::types::Value::Real(f) => f.max(0.0) as u64,
                rusqlite::types::Value::Text(t) => t.trim().parse().unwrap_or(0),
                _ => 0,
            },
            Ok(None) => {
                log::debug!(target: LOG_DB, "No timestamp found for client status report. It was not sent yet.");
                0
            }
            Err(_) => {
                log::debug!(target: LOG_DB, "Could not read last timestamp for client status report.");
                0
            }
        }
    }

    /// `setLastSentReportTimestamp(timestamp)`.
    pub fn set_last_sent_report_timestamp(&self, timestamp: u64) {
        let v = rusqlite::types::Value::Integer(i64::try_from(timestamp).unwrap_or(i64::MAX));
        if self.set_key_value(LAST_SENT_REPORT_TIMESTAMP, v).is_err() {
            log::warn!(target: LOG_DB, "Could not set last sent report timestamp from keyvalue table. No such record: {LAST_SENT_REPORT_TIMESTAMP}");
        }
    }

    fn get_status_names_hash(&self) -> Vec<u8> {
        match self.get_key_value(STATUS_NAMES_HASH) {
            Ok(Some(v)) => value_bytes((&v).into()),
            Ok(None) => {
                log::warn!(target: LOG_DB, "Could not get status names hash: ");
                Vec::new()
            }
            Err(_) => {
                log::warn!(target: LOG_DB, "Could not get status names hash. No such record: {STATUS_NAMES_HASH}");
                Vec::new()
            }
        }
    }

    /// `updateStatusNamesHash()`: the records are dropped when the list of
    /// statuses changed since they were written.
    fn update_status_names_hash(&self) -> bool {
        let mut concatenated = String::new();
        for s in ClientStatusReportingStatus::ALL {
            concatenated.push_str(s.status_string());
        }
        concatenated.push_str(&ClientStatusReportingStatus::COUNT.to_string());
        let current: Vec<u8> = Md5::digest(concatenated.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .into_bytes();
        if current != self.get_status_names_hash() {
            if self.delete_client_status_reporting_records().is_err() {
                return false;
            }
            if let Err(e) =
                self.set_key_value(STATUS_NAMES_HASH, rusqlite::types::Value::Blob(current))
            {
                log::warn!(target: LOG_DB, "Could not set status names hash. {e}");
                return false;
            }
        }
        true
    }
}

/// `QVariant::toByteArray()` of a column value.
fn value_bytes(v: rusqlite::types::ValueRef<'_>) -> Vec<u8> {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => Vec::new(),
        ValueRef::Integer(i) => i.to_string().into_bytes(),
        ValueRef::Real(f) => f.to_string().into_bytes(),
        ValueRef::Text(t) | ValueRef::Blob(t) => t.to_vec(),
    }
}

/// `ClientStatusReportingNetwork::classifyStatus(status)`.
pub fn classify_status(status: ClientStatusReportingStatus) -> &'static str {
    use ClientStatusReportingStatus as S;
    match status {
        S::DownloadErrorConflictCaseClash | S::DownloadErrorConflictInvalidCharacters => {
            STATUS_REPORT_CATEGORY_SYNC_CONFLICTS
        }
        S::DownloadErrorServerError
        | S::DownloadErrorVirtualFileHydrationFailure
        | S::UploadErrorServerError => STATUS_REPORT_CATEGORY_PROBLEMS,
        S::UploadErrorVirusDetected => STATUS_REPORT_CATEGORY_VIRUS,
        S::E2EeErrorGeneralError => STATUS_REPORT_CATEGORY_E2E_ERRORS,
    }
}

/// `ClientStatusReportingNetwork::prepareReport()`: empty when there is no
/// record.
pub fn prepare_report(records: &[ClientStatusReportingRecord]) -> Map<String, Value> {
    let mut report = Map::new();
    if records.is_empty() {
        return report;
    }
    for key in [
        STATUS_REPORT_CATEGORY_SYNC_CONFLICTS,
        STATUS_REPORT_CATEGORY_PROBLEMS,
        STATUS_REPORT_CATEGORY_VIRUS,
        STATUS_REPORT_CATEGORY_E2E_ERRORS,
    ] {
        report.insert(key.to_owned(), Value::Object(Map::new()));
    }
    let mut e2ee_errors = Map::new();
    let mut problems = Map::new();
    let mut sync_conflicts = Map::new();
    let mut virus_detected_errors = Map::new();
    let add_count = |m: &mut Map<String, Value>, record: &ClientStatusReportingRecord| {
        let initial = m.get("count").and_then(Value::as_i64).unwrap_or(0);
        m.insert("count".to_owned(), json!(initial + record.num_occurences));
        m.insert("oldest".to_owned(), json!(record.last_occurence));
    };
    for record in records {
        let Some(status) = ClientStatusReportingStatus::from_number(record.status) else {
            log::warn!(target: LOG_NET, "Could not classify status:");
            continue;
        };
        let category_key = classify_status(status);
        match category_key {
            STATUS_REPORT_CATEGORY_E2E_ERRORS => {
                add_count(&mut e2ee_errors, record);
                report.insert(category_key.to_owned(), Value::Object(e2ee_errors.clone()));
            }
            STATUS_REPORT_CATEGORY_PROBLEMS => {
                problems.insert(
                    String::from_utf8_lossy(&record.name).into_owned(),
                    json!({"count": record.num_occurences, "oldest": record.last_occurence}),
                );
                report.insert(category_key.to_owned(), Value::Object(problems.clone()));
            }
            STATUS_REPORT_CATEGORY_SYNC_CONFLICTS => {
                add_count(&mut sync_conflicts, record);
                report.insert(
                    category_key.to_owned(),
                    Value::Object(sync_conflicts.clone()),
                );
            }
            _ => {
                add_count(&mut virus_detected_errors, record);
                report.insert(
                    category_key.to_owned(),
                    Value::Object(virus_detected_errors.clone()),
                );
            }
        }
    }
    report
}

/// `HttpErrorCodeNone`, `Success`, `SuccessCreated`, `SuccessNoContent`.
fn is_success_code(code: i64) -> bool {
    matches!(code, 0 | 200 | 201 | 204)
}

/// `ClientStatusReportingNetwork::sendReportToServer()`.
pub async fn send_report_to_server(account: &Account, database: &ClientStatusReportingDatabase) {
    let last_sent_report_time = database.get_last_sent_report_timestamp();
    if now_ms().saturating_sub(last_sent_report_time)
        < REPORT_SEND_INTERVAL_MS.load(Ordering::Relaxed)
    {
        log::debug!(target: LOG_NET, "Skipped sending client status report because interval is not met");
        return;
    }
    let report = prepare_report(&database.get_client_status_reporting_records());
    if report.is_empty() {
        log::debug!(target: LOG_NET, "Skipped sending client status report because no records are queued");
        return;
    }
    let (json, status_code, _reply) = nc_dav::jobs::json_api_request(
        account,
        http::Method::PUT,
        "ocs/v2.php/apps/security_guard/diagnostics",
        Some(&Value::Object(report)),
        &nc_dav::JobOptions::default(),
    )
    .await;
    let status_code = i64::from(status_code);
    if is_success_code(status_code) {
        // A request that failed without an HTTP status gives 0 (and no JSON)
        // here, which upstream counts as a success too.
        let code_from_json = json
            .as_ref()
            .and_then(|j| j["ocs"]["meta"]["statuscode"].as_i64())
            .unwrap_or(0);
        if is_success_code(code_from_json) {
            report_to_server_sent_successfully(database);
            return;
        }
        log::warn!(target: LOG_NET, "Received error when sending client status report statusCode: {status_code} codeFromJson: {code_from_json}");
    }
}

/// `reportToServerSentSuccessfully()`.
fn report_to_server_sent_successfully(database: &ClientStatusReportingDatabase) {
    log::info!(target: LOG_NET, "Succesfully sent client status report");
    if database.delete_client_status_reporting_records().is_err() {
        log::warn!(target: LOG_NET, "Could not delete records after sending the report");
    }
    database.set_last_sent_report_timestamp(now_ms());
}

/// `ClientStatusReporting`: the database and the send timer of one account.
pub struct ClientStatusReporting {
    database: Arc<ClientStatusReportingDatabase>,
    timer: Option<tokio::task::AbortHandle>,
}

impl Drop for ClientStatusReporting {
    fn drop(&mut self) {
        if let Some(t) = self.timer.take() {
            t.abort();
        }
    }
}

impl ClientStatusReporting {
    /// `ClientStatusReporting(account)`: opens the database at `db_path`
    /// and starts the send timer (`ClientStatusReportingNetwork::init`)
    /// on the current tokio runtime. `None` when the database cannot be set
    /// up.
    pub fn new(account: &Arc<Account>, db_path: &Path) -> Option<Self> {
        let database = Arc::new(ClientStatusReportingDatabase::open(db_path)?);
        let timer = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let account: Weak<Account> = Arc::downgrade(account);
                let db = database.clone();
                let interval = Duration::from_millis(
                    CLIENT_STATUS_REPORTING_TRY_SEND_TIMER_INTERVAL.load(Ordering::Relaxed),
                );
                Some(
                    handle
                        .spawn(async move {
                            let mut ticker = tokio::time::interval_at(
                                tokio::time::Instant::now() + interval,
                                interval,
                            );
                            loop {
                                ticker.tick().await;
                                let Some(account) = account.upgrade() else {
                                    return;
                                };
                                send_report_to_server(&account, &db).await;
                            }
                        })
                        .abort_handle(),
                )
            }
            Err(_) => {
                log::warn!(target: LOG_NET, "Could not send client status report to server. Status reporting is not initialized");
                None
            }
        };
        Some(Self { database, timer })
    }

    pub fn database(&self) -> &ClientStatusReportingDatabase {
        &self.database
    }
}

impl ClientStatusReporter for ClientStatusReporting {
    fn report_client_status(&self, status: ClientStatusReportingStatus) {
        let record = ClientStatusReportingRecord {
            name: status.status_string().as_bytes().to_vec(),
            status: status as i64,
            num_occurences: 0,
            last_occurence: now_ms() as i64,
        };
        if let Err(e) = self.database.set_client_status_reporting_record(&record) {
            log::warn!(target: LOG, "Could not report client status: {e}");
        }
    }
}

/// Lets `account` report client statuses when the server enables it; the
/// databases go to `config_dir` (`ConfigFile().configPath()`).
pub fn install(account: &Arc<Account>, config_dir: PathBuf) {
    account.enable_client_status_reporting(Arc::new(move |account| {
        let path = make_db_path(&config_dir, account);
        ClientStatusReporting::new(account, &path)
            .map(|r| Arc::new(r) as Arc<dyn ClientStatusReporter>)
    }));
}

/// `PropagateItemJob::reportClientStatuses()`: what a finished item
/// reports (called by `PropagateItemJob::done` with the new status).
pub fn report_client_statuses(account: &Account, item: &crate::item::SyncFileItem) {
    use crate::item::{Direction, Status};
    use ClientStatusReportingStatus as S;
    let code = item.http_error_code;
    if item.status == Status::FileNameClash {
        if item.direction != Direction::Up {
            account.report_client_status(S::DownloadErrorConflictInvalidCharacters);
        }
    } else if item.status == Status::FileNameInvalid {
        account.report_client_status(S::DownloadErrorConflictInvalidCharacters);
    } else if !is_success_code(i64::from(code)) {
        if item.direction == Direction::Up {
            // HttpErrorCodeBadRequest, HttpErrorCodeUnsupportedMediaType
            let is_code_bad_req_or_unsupported_media_type = code == 400 || code == 415;
            let is_exception_info_present =
                !item.error_exception_name.is_empty() && !item.error_exception_message.is_empty();
            if is_code_bad_req_or_unsupported_media_type
                && is_exception_info_present
                && item.error_exception_name.contains("UnsupportedMediaType")
                && item
                    .error_exception_message
                    .to_lowercase()
                    .contains("virus")
            {
                account.report_client_status(S::UploadErrorVirusDetected);
            } else {
                account.report_client_status(S::UploadErrorServerError);
            }
        } else {
            account.report_client_status(S::DownloadErrorServerError);
        }
    }
}
