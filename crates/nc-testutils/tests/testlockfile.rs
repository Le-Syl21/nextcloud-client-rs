// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream test/testlockfile.cpp (nextcloud/desktop v34.0.5).
//
// `QSignalSpy::wait()` on the account's `lockFileSuccess` / `lockFileError`
// or on the job's `finishedWithoutError` / `finishedWithError` becomes
// running the request to its end: `set_lock_file_state` and
// `LockFileJob::start` resolve to `Ok` (success) or `Err` (error).
// `SyncEngine::lockFileDetected` is the engine callback of the same name.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use nc_journal::csync::JournalDbEncryptionStatus;
use nc_journal::journal::{ErrorCategory as BlacklistCategory, SyncJournalFileRecord};
use nc_sync::account_lock::{file_can_be_unlocked, file_lock_status, set_lock_file_state};
use nc_sync::discovery::LocalDiscoveryStyle;
use nc_sync::filesystem::{self, FileLockingType, lock_file_target_file_path};
use nc_sync::lock_file_jobs::{LockFileJob, LockFileJobError};
use nc_sync::utility::current_secs_since_epoch;
use nc_sync::{LockOwnerType, LockStatus};
use nc_testutils::server::error_reply;
use nc_testutils::{
    FakeFolder, FakeReply, FileInfo, FileModifier, ItemCompletedSpy, LockChange, LockState,
    run_event_loop,
};
use serde_json::json;

fn file_record(fake_folder: &FakeFolder, path: &str) -> Option<SyncJournalFileRecord> {
    fake_folder
        .sync_journal()
        .get_file_record(path.as_bytes())
        .expect("getFileRecord")
}

/// `fakeFolder.account()->setLockFileState(QStringLiteral("/") + file,
/// QStringLiteral("/"), fakeFolder.localPath(), {}, &fakeFolder.syncJournal(),
/// status, type)`, run to its signal.
fn set_lock_file_state_and_wait(
    fake_folder: &FakeFolder,
    file: &str,
    status: LockStatus,
    owner_type: LockOwnerType,
) -> Result<(), String> {
    let request = set_lock_file_state(
        fake_folder.account(),
        &format!("/{file}"),
        "/",
        &fake_folder.local_path(),
        "",
        &fake_folder.sync_journal_handle(),
        status,
        owner_type,
    )
    .expect("no request running for the file");
    run_event_loop(request)
}

/// `new OCC::LockFileJob(fakeFolder.account(), &fakeFolder.syncJournal(),
/// QStringLiteral("/") + file, QStringLiteral("/"), fakeFolder.localPath(),
/// {}, status, UserLock)` started and run to its signal.
fn run_lock_file_job(
    fake_folder: &FakeFolder,
    file: &str,
    status: LockStatus,
) -> Result<(), LockFileJobError> {
    let job = LockFileJob::new(
        fake_folder.account().clone(),
        fake_folder.sync_journal_handle(),
        &format!("/{file}"),
        "/",
        &fake_folder.local_path(),
        "",
        status,
        LockOwnerType::UserLock,
    );
    run_event_loop(job.start())
}

/// Answers LOCK and/or UNLOCK with `FakeErrorReply(code, body)`.
fn override_lock_verbs(
    fake_folder: &FakeFolder,
    lock: Option<(u16, &'static [u8])>,
    unlock: Option<(u16, &'static [u8])>,
) {
    fake_folder.set_server_override(move |request, _| {
        let answer = match request.method().as_str() {
            "LOCK" => lock,
            "UNLOCK" => unlock,
            _ => None,
        }?;
        Some(FakeReply::from(error_reply(
            answer.0,
            Bytes::from_static(answer.1),
        )))
    });
}

const LOCKED_HTTP_ERROR_CODE: u16 = 423;
const PRECONDITION_FAILED_HTTP_ERROR_CODE: u16 = 412;

const UNLOCKED_JOHN_REPLY: &[u8] = b"<?xml version=\"1.0\"?>\n\
<d:prop xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">\n \
<nc:lock/>\n \
<nc:lock-owner-type>0</nc:lock-owner-type>\n \
<nc:lock-owner>john</nc:lock-owner>\n \
<nc:lock-owner-displayname>John Doe</nc:lock-owner-displayname>\n \
<nc:lock-owner-editor>john</nc:lock-owner-editor>\n \
<nc:lock-time>1650619678</nc:lock-time>\n \
<nc:lock-timeout>300</nc:lock-timeout>\n \
<nc:lock-token>files_lock/310997d7-0aae-4e48-97e1-eeb6be6e2202</nc:lock-token>\n\
</d:prop>\n";

const LOCKED_BY_JOHN_USER_REPLY: &[u8] = b"<?xml version=\"1.0\"?>\n\
<d:prop xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">\n \
<nc:lock>1</nc:lock>\n \
<nc:lock-owner-type>0</nc:lock-owner-type>\n \
<nc:lock-owner>john</nc:lock-owner>\n \
<nc:lock-owner-displayname>John Doe</nc:lock-owner-displayname>\n \
<nc:lock-owner-editor>john</nc:lock-owner-editor>\n \
<nc:lock-time>1650619678</nc:lock-time>\n \
<nc:lock-timeout>300</nc:lock-timeout>\n \
<nc:lock-token>files_lock/310997d7-0aae-4e48-97e1-eeb6be6e2202</nc:lock-token>\n\
</d:prop>\n";

const LOCKED_BY_JOHN_APP_REPLY: &[u8] = b"<?xml version=\"1.0\"?>\n\
<d:prop xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">\n \
<nc:lock>1</nc:lock>\n \
<nc:lock-owner-type>1</nc:lock-owner-type>\n \
<nc:lock-owner>john</nc:lock-owner>\n \
<nc:lock-owner-displayname>John Doe</nc:lock-owner-displayname>\n \
<nc:lock-owner-editor>Text</nc:lock-owner-editor>\n \
<nc:lock-time>1650619678</nc:lock-time>\n \
<nc:lock-timeout>300</nc:lock-timeout>\n \
<nc:lock-token>files_lock/310997d7-0aae-4e48-97e1-eeb6be6e2202</nc:lock-token>\n\
</d:prop>\n";

const LOCKED_BY_ALICE_REPLY: &[u8] = b"<?xml version=\"1.0\"?>\n\
<d:prop xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">\n \
<nc:lock>1</nc:lock>\n \
<nc:lock-owner-type>0</nc:lock-owner-type>\n \
<nc:lock-owner>alice</nc:lock-owner>\n \
<nc:lock-owner-displayname>Alice Doe</nc:lock-owner-displayname>\n \
<nc:lock-owner-editor>Text</nc:lock-owner-editor>\n \
<nc:lock-time>1650619678</nc:lock-time>\n \
<nc:lock-timeout>300</nc:lock-timeout>\n \
<nc:lock-token>files_lock/310997d7-0aae-4e48-97e1-eeb6be6e2202</nc:lock-token>\n\
</d:prop>\n";

/// `modifyLockState(path, lockState, lockType, lockOwner, lockOwnerId,
/// lockEditorId, lockTime, lockTimeout)`.
fn lock_change(
    lock_state: LockState,
    lock_type: i32,
    lock_owner: &str,
    lock_owner_id: &str,
    lock_editor_id: &str,
    lock_time: i64,
    lock_timeout: u64,
) -> LockChange {
    LockChange {
        lock_state,
        lock_type,
        lock_owner: lock_owner.to_owned(),
        lock_owner_id: lock_owner_id.to_owned(),
        lock_editor_id: lock_editor_id.to_owned(),
        lock_time: lock_time.max(0) as u64,
        lock_timeout,
    }
}

fn unlocked() -> LockChange {
    LockChange {
        lock_state: LockState::FileUnlocked,
        ..Default::default()
    }
}

/// `QSignalSpy lockFileDetectedNewlyUploadedSpy(&fakeFolder.syncEngine(),
/// &OCC::SyncEngine::lockFileDetected)`.
fn lock_file_detected_spy(fake_folder: &mut FakeFolder) -> Arc<Mutex<Vec<String>>> {
    let spy = Arc::new(Mutex::new(Vec::new()));
    let s = spy.clone();
    fake_folder.sync_engine().callbacks().lock_file_detected = Some(Box::new(move |f: &str| {
        s.lock().unwrap().push(f.to_owned());
    }));
    spy
}

// initTestCase: n/a (Qt logger / QStandardPaths test mode).

#[test]
fn test_lock_file_lock_file_lock_success() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    let result = set_lock_file_state_and_wait(
        &fake_folder,
        test_file_name,
        LockStatus::LockedItem,
        LockOwnerType::UserLock,
    );
    assert_eq!(result, Ok(()));
}

#[test]
fn test_lock_file_lock_file_lock_error() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((LOCKED_HTTP_ERROR_CODE, UNLOCKED_JOHN_REPLY)),
        None,
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    let result = set_lock_file_state_and_wait(
        &fake_folder,
        test_file_name,
        LockStatus::LockedItem,
        LockOwnerType::UserLock,
    );
    assert!(result.is_err());
}

#[test]
fn test_lock_file_file_lock_status_query_lock_status() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    let result = set_lock_file_state_and_wait(
        &fake_folder,
        test_file_name,
        LockStatus::LockedItem,
        LockOwnerType::UserLock,
    );
    assert_eq!(result, Ok(()));

    let lock_status = file_lock_status(fake_folder.sync_journal(), test_file_name);
    assert_eq!(lock_status, LockStatus::LockedItem);
}

#[test]
fn test_lock_file_file_can_be_unlocked_can_unlock() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    let result = set_lock_file_state_and_wait(
        &fake_folder,
        test_file_name,
        LockStatus::LockedItem,
        LockOwnerType::UserLock,
    );
    assert_eq!(result, Ok(()));

    let lock_status = file_can_be_unlocked(
        fake_folder.account(),
        fake_folder.sync_journal(),
        test_file_name,
    );
    assert!(lock_status);
}

fn assert_john_doe_lock(record: &SyncJournalFileRecord) {
    assert!(record.lockstate.locked);
    assert_eq!(record.lockstate.lock_editor_app, "");
    assert_eq!(record.lockstate.lock_owner_display_name, "John Doe");
    assert_eq!(record.lockstate.lock_owner_id, "admin");
    assert_eq!(
        record.lockstate.lock_owner_type,
        LockOwnerType::UserLock as i64
    );
    assert_eq!(record.lockstate.lock_time, 1234560);
    assert_eq!(record.lockstate.lock_timeout, 1800);
}

#[test]
fn test_lock_file_lock_file_job_success() {
    let test_file_name = "file.txt";
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert_eq!(
        run_lock_file_job(&fake_folder, test_file_name, LockStatus::LockedItem),
        Ok(())
    );

    let file_record = file_record(&fake_folder, test_file_name).expect("record");
    assert_john_doe_lock(&file_record);

    assert!(fake_folder.sync_once());
}

#[test]
fn test_lock_file_lock_file_unlock_file_job_success() {
    let test_file_name = "file.txt";
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert_eq!(
        run_lock_file_job(&fake_folder, test_file_name, LockStatus::LockedItem),
        Ok(())
    );

    assert!(fake_folder.sync_once());

    assert_eq!(
        run_lock_file_job(&fake_folder, test_file_name, LockStatus::UnlockedItem),
        Ok(())
    );

    let file_record = file_record(&fake_folder, test_file_name).expect("record");
    assert!(!file_record.lockstate.locked);

    assert!(fake_folder.sync_once());
}

#[test]
fn test_lock_file_lock_file_already_locked_by_user() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((LOCKED_HTTP_ERROR_CODE, LOCKED_BY_JOHN_USER_REPLY)),
        Some((
            PRECONDITION_FAILED_HTTP_ERROR_CODE,
            LOCKED_BY_JOHN_USER_REPLY,
        )),
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(run_lock_file_job(&fake_folder, test_file_name, LockStatus::LockedItem).is_err());
}

#[test]
fn test_lock_file_lock_file_already_locked_by_app() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((LOCKED_HTTP_ERROR_CODE, LOCKED_BY_JOHN_APP_REPLY)),
        Some((
            PRECONDITION_FAILED_HTTP_ERROR_CODE,
            LOCKED_BY_JOHN_APP_REPLY,
        )),
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(run_lock_file_job(&fake_folder, test_file_name, LockStatus::LockedItem).is_err());
}

#[test]
fn test_lock_file_unlock_file_already_unlocked() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((LOCKED_HTTP_ERROR_CODE, UNLOCKED_JOHN_REPLY)),
        Some((PRECONDITION_FAILED_HTTP_ERROR_CODE, UNLOCKED_JOHN_REPLY)),
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert_eq!(
        run_lock_file_job(&fake_folder, test_file_name, LockStatus::UnlockedItem),
        Ok(())
    );
}

#[test]
fn test_lock_file_unlock_file_locked_by_someone_else() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((LOCKED_HTTP_ERROR_CODE, LOCKED_BY_ALICE_REPLY)),
        Some((LOCKED_HTTP_ERROR_CODE, LOCKED_BY_ALICE_REPLY)),
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(run_lock_file_job(&fake_folder, test_file_name, LockStatus::UnlockedItem).is_err());
}

#[test]
fn test_lock_file_lock_file_job_error() {
    let test_file_name = "file.txt";
    const INTERNAL_SERVER_ERROR_HTTP_ERROR_CODE: u16 = 500;

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((INTERNAL_SERVER_ERROR_HTTP_ERROR_CODE, b"")),
        Some((INTERNAL_SERVER_ERROR_HTTP_ERROR_CODE, b"")),
    );

    fake_folder.local_modifier().insert("file.txt", 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(run_lock_file_job(&fake_folder, test_file_name, LockStatus::LockedItem).is_err());

    assert!(fake_folder.sync_once());

    assert!(run_lock_file_job(&fake_folder, test_file_name, LockStatus::UnlockedItem).is_err());

    assert!(fake_folder.sync_once());
}

#[test]
fn test_lock_file_lock_file_precondition_failed_error() {
    let test_file_name = "file.txt";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    override_lock_verbs(
        &fake_folder,
        Some((PRECONDITION_FAILED_HTTP_ERROR_CODE, LOCKED_BY_ALICE_REPLY)),
        Some((PRECONDITION_FAILED_HTTP_ERROR_CODE, LOCKED_BY_ALICE_REPLY)),
    );

    fake_folder
        .local_modifier()
        .insert(test_file_name, 64, b'W');

    assert!(fake_folder.sync_once());

    assert!(run_lock_file_job(&fake_folder, test_file_name, LockStatus::UnlockedItem).is_err());
}

// The engine's scheduled sync run timers are deadlines (see
// `nc_sync::scheduled_sync`): `QSignalSpy::wait(ms)` on `SyncEngine::finished`
// becomes "the next deadline is within `ms`", then sleeping until it,
// `fire_due` (the timer's `startSync()`) and a sync run.

#[test]
fn test_sync_locked_files_almost_expired() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::UnlockedItem
    );
    let file_record_before = file_record(&fake_folder, "A/a1").expect("record");
    assert!(!file_record_before.lockstate.locked);

    fake_folder.remote_modifier().modify_lock_state(
        "A/a1",
        lock_change(
            LockState::FileLocked,
            1,
            "Nextcloud Office",
            "",
            "richdocuments",
            current_secs_since_epoch() - 1220,
            1227,
        ),
    );

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::LockedItem
    );
    let file_record_locked = file_record(&fake_folder, "A/a1").expect("record");
    assert!(file_record_locked.lockstate.locked);

    std::thread::sleep(Duration::from_millis(5000));

    fake_folder
        .remote_modifier()
        .modify_lock_state("A/a1", unlocked());

    // QCOMPARE(spySyncCompleted.count(), 0): no sync before the timer fires.
    let deadline = fake_folder
        .sync_engine()
        .scheduled_sync_timers()
        .next_deadline()
        .expect("a scheduled sync run");
    // QVERIFY(spySyncCompleted.wait(4000))
    assert!(deadline <= Instant::now() + Duration::from_millis(4000));
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    assert!(
        fake_folder
            .sync_engine()
            .scheduled_sync_timers()
            .fire_due(Instant::now())
    );
    assert!(fake_folder.sync_once());

    let file_record_unlocked = file_record(&fake_folder, "A/a1").expect("record");
    assert!(!file_record_unlocked.lockstate.locked);
}

#[test]
fn test_sync_locked_files_no_expired_locked_files() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::UnlockedItem
    );
    let file_record_before = file_record(&fake_folder, "A/a1").expect("record");
    assert!(!file_record_before.lockstate.locked);

    fake_folder.remote_modifier().modify_lock_state(
        "A/a1",
        lock_change(
            LockState::FileLocked,
            1,
            "Nextcloud Office",
            "",
            "richdocuments",
            current_secs_since_epoch() - 1220,
            1226,
        ),
    );

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::LockedItem
    );
    let file_record_locked = file_record(&fake_folder, "A/a1").expect("record");
    assert!(file_record_locked.lockstate.locked);

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    std::thread::sleep(Duration::from_millis(1000));

    fake_folder
        .remote_modifier()
        .modify_lock_state("A/a1", unlocked());

    // QVERIFY(!spySyncCompleted.wait(3000)): no scheduled sync run is due
    // within 3 s.
    let deadline = fake_folder
        .sync_engine()
        .scheduled_sync_timers()
        .next_deadline();
    assert!(deadline.is_none_or(|d| d > Instant::now() + Duration::from_millis(3000)));
    std::thread::sleep(Duration::from_millis(3000));
    let fired = fake_folder
        .sync_engine()
        .scheduled_sync_timers()
        .fire_due(Instant::now());
    assert!(!fired);

    let file_record_unlocked = file_record(&fake_folder, "A/a1").expect("record");
    assert!(file_record_unlocked.lockstate.locked);
}

/// Counts GET and PUT requests (`nGET`, `nPUT`).
#[derive(Clone, Default)]
struct GetPutCounter {
    n_get: Arc<Mutex<i32>>,
    n_put: Arc<Mutex<i32>>,
    put_with_if_header: Arc<Mutex<bool>>,
    verify_lack_of_token_if_header: Arc<Mutex<bool>>,
}

impl GetPutCounter {
    fn install(&self, fake_folder: &FakeFolder) {
        let c = self.clone();
        fake_folder.set_server_override(move |request, _| {
            match request.method().as_str() {
                "PUT" => {
                    *c.n_put.lock().unwrap() += 1;
                    if *c.verify_lack_of_token_if_header.lock().unwrap()
                        && request.headers().contains_key("If")
                    {
                        // Q_ASSERT(false)
                        *c.put_with_if_header.lock().unwrap() = true;
                    }
                }
                "GET" => *c.n_get.lock().unwrap() += 1,
                _ => {}
            }
            None
        });
    }

    fn get(&self) -> i32 {
        *self.n_get.lock().unwrap()
    }

    fn put(&self) -> i32 {
        *self.n_put.lock().unwrap()
    }

    fn reset(&self) {
        *self.n_get.lock().unwrap() = 0;
        *self.n_put.lock().unwrap() = 0;
    }
}

#[test]
fn test_sync_locked_files() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let counter = GetPutCounter::default();
    counter.install(&fake_folder);

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    complete_spy.clear();
    assert!(fake_folder.sync_once());
    assert_eq!(counter.get(), 0);
    assert_eq!(counter.put(), 0);

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::UnlockedItem
    );
    let file_record_before = file_record(&fake_folder, "A/a1").expect("record");
    assert!(!file_record_before.lockstate.locked);

    {
        let mut remote = fake_folder.remote_modifier();
        remote.modify_lock_state(
            "A/a1",
            lock_change(
                LockState::FileLocked,
                1,
                "Nextcloud Office",
                "",
                "richdocuments",
                current_secs_since_epoch(),
                1226,
            ),
        );
        remote.set_mod_time_keep_etag("A/a1", SystemTime::now());
        remote.append_byte("A/a1");
    }

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(counter.get(), 1);
    assert_eq!(counter.put(), 0);

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::LockedItem
    );
    let file_record_locked = file_record(&fake_folder, "A/a1").expect("record");
    assert!(file_record_locked.lockstate.locked);

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(counter.get(), 1);
    assert_eq!(counter.put(), 0);

    let file_record_after = file_record(&fake_folder, "A/a1").expect("record");
    assert!(file_record_after.lockstate.locked);

    let expected_state = fake_folder.current_local_state();
    assert_eq!(fake_folder.current_remote_state(), expected_state);
}

#[test]
fn test_lock_file_locked_file_read_only_after_sync() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::UnlockedItem
    );
    let file_record_before = file_record(&fake_folder, "A/a1").expect("record");
    assert!(!file_record_before.lockstate.locked);

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let local_file = format!("{}A/a1", fake_folder.local_path());
    assert!(filesystem::is_writable(&local_file));

    {
        let mut remote = fake_folder.remote_modifier();
        remote.modify_lock_state(
            "A/a1",
            lock_change(
                LockState::FileLocked,
                1,
                "Nextcloud Office",
                "",
                "richdocuments",
                current_secs_since_epoch(),
                1226,
            ),
        );
        remote.set_mod_time_keep_etag("A/a1", SystemTime::now());
        remote.append_byte("A/a1");
    }

    complete_spy.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        complete_spy.find_item("A/a1").locked,
        LockStatus::LockedItem
    );
    let file_record_locked = file_record(&fake_folder, "A/a1").expect("record");
    assert!(file_record_locked.lockstate.locked);

    let expected_state = fake_folder.current_local_state();
    assert_eq!(fake_folder.current_remote_state(), expected_state);

    assert!(!filesystem::is_writable(&local_file));
}

/// The body of the `testLockFile_lockFile_detect_newly_uploaded*` tests:
/// the lock file and the document appear in `documents`, with
/// `manual_excludes` (the default exclude list entries for the lock file)
/// and the files locking capability.
fn detect_newly_uploaded(
    test_file_name: &str,
    test_lock_file_name: &str,
    manual_excludes: &[&str],
) {
    let test_documents_dir_name = "documents";

    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.local_modifier().mkdir(test_documents_dir_name);
    for exclude in manual_excludes {
        fake_folder
            .sync_engine()
            .excluded_files()
            .add_manual_exclude(exclude);
    }

    fake_folder.set_capabilities(json!({"files": {"locking": "1.0"}}));
    let lock_file_detected_newly_uploaded_spy = lock_file_detected_spy(&mut fake_folder);

    fake_folder.local_modifier().insert(
        &format!("{test_documents_dir_name}/{test_lock_file_name}"),
        64,
        b'W',
    );
    fake_folder.local_modifier().insert(
        &format!("{test_documents_dir_name}/{test_file_name}"),
        64,
        b'W',
    );

    assert!(fake_folder.sync_once());

    assert_eq!(
        lock_file_detected_newly_uploaded_spy.lock().unwrap().len(),
        1
    );
}

#[test]
fn test_lock_file_lock_file_detect_newly_uploaded() {
    detect_newly_uploaded("document.docx", ".~lock.document.docx#", &[]);
}

#[test]
fn test_lock_file_lock_file_detect_newly_uploaded_autocad() {
    // AutoCAD: opening a .dwg creates both .dwl and .dwl2 lock files.
    // Both share the document's base name — the guarded document is
    // resolved by replacing the extension with .dwg.
    // AutoCAD lock files are excluded from sync by the default exclude list;
    // the FakeFolder does not load it, so replicate the exclusion here.
    detect_newly_uploaded("floorplan.dwg", "floorplan.dwl", &["*.dwl", "*.dwl2"]);
}

#[test]
fn test_lock_file_lock_file_detect_newly_uploaded_adobe_idlk() {
    // InDesign: opening a .indd creates `~{base}~{token}(.idlk`, which carries
    // only the document's base name. The guarded document is resolved by
    // matching a sibling .indd/.icml file in the same directory.
    // Adobe lock files are excluded from sync by the default exclude list;
    // the FakeFolder does not load it, so replicate the exclusion here.
    detect_newly_uploaded("document.indd", "~document~0kjyv(.idlk", &["*.idlk"]);
}

/// `QFile{path}.open(QIODevice::WriteOnly)`: creates the file.
fn create(path: &str) {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .expect("create file");
}

#[test]
fn test_lock_file_auto_cad_lock_file_target_file_path_resolution() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let dir_path = format!("{}/", dir.path().to_str().unwrap());

    // .dwl lock file guards the .dwg document with the same base name.
    let dwg_path = format!("{dir_path}Drawing.dwg");
    let dwl_path = format!("{dir_path}Drawing.dwl");
    create(&dwg_path);
    create(&dwl_path);

    let dwl_resolved = lock_file_target_file_path(&dwl_path, "");
    assert_eq!(dwl_resolved.path, dwg_path);
    assert_eq!(dwl_resolved.locking_type, FileLockingType::Locked);

    // .dwl2 lock file also guards the same .dwg document.
    let dwl2_path = format!("{dir_path}Drawing.dwl2");
    create(&dwl2_path);
    let dwl2_resolved = lock_file_target_file_path(&dwl2_path, "");
    assert_eq!(dwl2_resolved.path, dwg_path);
    assert_eq!(dwl2_resolved.locking_type, FileLockingType::Locked);

    // Deleting only .dwl2 while .dwl still exists must NOT unlock the document.
    std::fs::remove_file(&dwl2_path).expect("remove");
    let dwl2_deleted = lock_file_target_file_path(&dwl2_path, "");
    assert_eq!(dwl2_deleted.path, dwg_path);
    assert_eq!(dwl2_deleted.locking_type, FileLockingType::Unset);

    // Recreate .dwl2, then delete only .dwl while .dwl2 still exists.
    // This must also NOT unlock the document.
    create(&dwl2_path);
    std::fs::remove_file(&dwl_path).expect("remove");
    let dwl_deleted = lock_file_target_file_path(&dwl_path, "");
    assert_eq!(dwl_deleted.path, dwg_path);
    assert_eq!(dwl_deleted.locking_type, FileLockingType::Unset);

    // Removing the remaining .dwl2 now unlocks the document (both are gone).
    std::fs::remove_file(&dwl2_path).expect("remove");
    let dwl_unlocked = lock_file_target_file_path(&dwl2_path, "");
    assert_eq!(dwl_unlocked.path, dwg_path);
    assert_eq!(dwl_unlocked.locking_type, FileLockingType::Unlocked);

    // A non-lock file resolves to nothing.
    let non_lock = lock_file_target_file_path(&format!("{dir_path}notes.txt"), "");
    assert!(non_lock.path.is_empty());
    assert_eq!(non_lock.locking_type, FileLockingType::Unset);

    // An AutoCAD lock file with no matching .dwg sibling resolves to nothing.
    let orphan_dwl_path = format!("{dir_path}orphan.dwl");
    create(&orphan_dwl_path);
    let orphan = lock_file_target_file_path(&orphan_dwl_path, "");
    assert!(orphan.path.is_empty());
}

#[test]
fn test_lock_file_lock_file_detect_newly_uploaded_adobe_prlock() {
    // Premiere Pro: opening a .prproj creates `{base}.prlock`.
    detect_newly_uploaded("project.prproj", "project.prlock", &["*.prlock"]);
}

#[test]
fn test_lock_file_adobe_lock_file_target_file_path_resolution() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let dir_path = format!("{}/", dir.path().to_str().unwrap());

    // InDesign: `~{base}~{token}(.idlk` guards `{base}.indd`.
    let indd_path = format!("{dir_path}Test.indd");
    let idlk_path = format!("{dir_path}~Test~0kjyv(.idlk");
    create(&indd_path);
    create(&idlk_path);

    let idlk_resolved = lock_file_target_file_path(&idlk_path, "");
    assert_eq!(idlk_resolved.path, indd_path);
    assert_eq!(idlk_resolved.locking_type, FileLockingType::Locked);

    // Removing the lock file marks the document as unlocked (sibling still present).
    std::fs::remove_file(&idlk_path).expect("remove");
    let idlk_unlocked = lock_file_target_file_path(&idlk_path, "");
    assert_eq!(idlk_unlocked.path, indd_path);
    assert_eq!(idlk_unlocked.locking_type, FileLockingType::Unlocked);

    // Premiere Pro: `{base}.prlock` guards `{base}.prproj`.
    let prproj_path = format!("{dir_path}project.prproj");
    let prlock_path = format!("{dir_path}project.prlock");
    create(&prproj_path);
    create(&prlock_path);

    let prlock_resolved = lock_file_target_file_path(&prlock_path, "");
    assert_eq!(prlock_resolved.path, prproj_path);
    assert_eq!(prlock_resolved.locking_type, FileLockingType::Locked);

    // InCopy story: `.idlk` may guard an `.icml` when no `.indd` sibling exists.
    let icml_path = format!("{dir_path}story.icml");
    let idlk2_path = format!("{dir_path}~story~ab12cd(.idlk");
    create(&icml_path);
    create(&idlk2_path);
    let idlk2_resolved = lock_file_target_file_path(&idlk2_path, "");
    assert_eq!(idlk2_resolved.path, icml_path);
    assert_eq!(idlk2_resolved.locking_type, FileLockingType::Locked);

    // A non-lock file resolves to nothing.
    let non_lock = lock_file_target_file_path(&format!("{dir_path}notes.txt"), "");
    assert!(non_lock.path.is_empty());
    assert_eq!(non_lock.locking_type, FileLockingType::Unset);

    // A lock file with no matching sibling document resolves to nothing.
    let orphan_idlk_path = format!("{dir_path}~orphan~zz(.idlk");
    create(&orphan_idlk_path);
    let orphan = lock_file_target_file_path(&orphan_idlk_path, "");
    assert!(orphan.path.is_empty());

    // An .idlk file whose name does not match the InDesign `~base~token(.idlk`
    // pattern resolves to nothing — the regex fails, base name is nullopt.
    let malformed_idlk_names = [
        "Test.idlk",        // missing leading ~ and token
        "~Test.idlk",       // missing ~token(
        "~Test~token.idlk", // missing trailing (
        "Test~token(.idlk", // missing leading ~
    ];
    for malformed_name in malformed_idlk_names {
        let malformed_path = format!("{dir_path}{malformed_name}");
        create(&malformed_path);
        let resolved = lock_file_target_file_path(&malformed_path, "");
        assert!(
            resolved.path.is_empty(),
            "Expected empty path for malformed idlk name: {malformed_name}"
        );
        let _ = std::fs::remove_file(&malformed_path);
    }

    // A non-Adobe, non-Office extension resolves to nothing.
    let unknown_ext_path = format!("{dir_path}file.unknown");
    create(&unknown_ext_path);
    let unknown_ext = lock_file_target_file_path(&unknown_ext_path, "");
    assert!(unknown_ext.path.is_empty());
    assert_eq!(unknown_ext.locking_type, FileLockingType::Unset);
}

#[test]
fn test_lock_file_lock_file_detect_newly_uploaded_affinity_afphoto() {
    detect_newly_uploaded(
        "Screenshot.afphoto",
        "Screenshot.afphoto~lock~",
        &["*~lock~"],
    );
}

#[test]
fn test_lock_file_lock_file_detect_newly_uploaded_affinity_afdesign() {
    detect_newly_uploaded("Icon.afdesign", "Icon.afdesign~lock~", &["*~lock~"]);
}

#[test]
fn test_lock_file_affinity_lock_file_target_file_path_resolution() {
    // Upstream uses the fixed directory /tmp/affinityTestDir/; a temporary
    // directory is used instead.
    let dir = tempfile::tempdir().expect("temporary directory");
    let dir_path = format!("{}/affinityTestDir/", dir.path().to_str().unwrap());

    assert!(filesystem::mkpath(&dir_path));

    // Create a document and an Affinity lock file for it.
    let doc_path = format!("{dir_path}Screenshot.afphoto");
    let lock_path = format!("{dir_path}Screenshot.afphoto~lock~");
    create(&doc_path);
    create(&lock_path);

    let resolved = lock_file_target_file_path(&lock_path, "");
    assert_eq!(resolved.path, doc_path);
    assert_eq!(resolved.locking_type, FileLockingType::Locked);

    // Removing the lock file should resolve to Unlocked.
    let _ = std::fs::remove_file(&lock_path);
    let unlocked = lock_file_target_file_path(&lock_path, "");
    assert_eq!(unlocked.path, doc_path);
    assert_eq!(unlocked.locking_type, FileLockingType::Unlocked);

    // A non-Affinity file should not match.
    let non_lock = lock_file_target_file_path(&format!("{dir_path}notes.txt"), "");
    assert!(non_lock.path.is_empty());

    // An Affinity lock file with no matching document resolves to nothing.
    let orphan_lock_path = format!("{dir_path}Orphan.afpub~lock~");
    create(&orphan_lock_path);
    let orphan = lock_file_target_file_path(&orphan_lock_path, "");
    assert!(orphan.path.is_empty());
}

#[test]
fn test_lock_file_verify_e2ee_files_use_correct_path() {
    let e2ee_root = "encrypted";
    let cleartext_file_path = "encrypted/document.odt";
    let encrypted_file_path = "encrypted/1e4c70c057994f9daf7bbab71b046d5b";

    let mut fake_folder = FakeFolder::new(FileInfo::default());

    fake_folder.local_modifier().mkdir(e2ee_root);
    fake_folder.remote_modifier().mkdir(e2ee_root);
    fake_folder
        .local_modifier()
        .insert(cleartext_file_path, 64, b'W');

    assert!(fake_folder.sync_once());

    // modify local entry for the file to be locked to pretend it's E2E encrypted
    let mut record = file_record(&fake_folder, cleartext_file_path).expect("record");
    record.e2e_encryption_status = JournalDbEncryptionStatus::EncryptedMigratedV2_0;
    record.e2e_mangled_name = encrypted_file_path.as_bytes().to_vec();
    record.path = cleartext_file_path.as_bytes().to_vec();
    assert!(fake_folder.sync_journal().set_file_record(&record).is_ok());

    // do something similar on the remote -- the encrypted file has a different name
    {
        let mut remote = fake_folder.remote_modifier();
        remote.rename(cleartext_file_path, encrypted_file_path);
        remote.set_e2ee(encrypted_file_path, true);
    }

    // another sync run should not fail now, even with our pretended E2Ee setup :-)
    assert!(fake_folder.sync_once());

    let lock_request_path = Arc::new(Mutex::new(String::new()));
    let p = lock_request_path.clone();
    fake_folder.set_server_override(move |request, _| {
        if request.method().as_str() == "LOCK" {
            let mut p = p.lock().unwrap();
            assert!(p.is_empty());
            *p = percent_decode(request.uri().path());
        }
        None
    });

    assert_eq!(
        run_lock_file_job(&fake_folder, cleartext_file_path, LockStatus::LockedItem),
        Ok(())
    );

    // expect the path of the LOCK request to have used the mangled name
    let lock_request_path = lock_request_path.lock().unwrap().clone();
    assert!(!lock_request_path.is_empty());
    assert!(lock_request_path.contains(encrypted_file_path));
    assert!(!lock_request_path.contains(cleartext_file_path));

    let file_record = file_record(&fake_folder, cleartext_file_path).expect("record");
    assert!(file_record.is_e2e_encrypted());
    assert_eq!(file_record.e2e_mangled_name, encrypted_file_path.as_bytes());
    assert_john_doe_lock(&file_record);

    assert!(fake_folder.sync_once());
}

/// `QUrl::path()` (decoded).
fn percent_decode(path: &str) -> String {
    percent_encoding::percent_decode_str(path)
        .decode_utf8_lossy()
        .into_owned()
}

#[test]
fn test_upload_locked_files_in_deleted_folder() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    let counter = GetPutCounter::default();
    counter.install(&fake_folder);

    let complete_spy = ItemCompletedSpy::new(&mut fake_folder);

    let clean_up_helper = || {
        counter.reset();
        complete_spy.clear();
    };

    fake_folder.local_modifier().mkdir("parent");

    clean_up_helper();
    assert!(fake_folder.sync_once());
    assert_eq!(counter.get(), 0);
    assert_eq!(counter.put(), 0);

    fake_folder.local_modifier().mkdir("parent/child");

    clean_up_helper();
    assert!(fake_folder.sync_once());
    assert_eq!(counter.get(), 0);
    assert_eq!(counter.put(), 0);

    fake_folder
        .local_modifier()
        .insert("parent/child/hello.odt", 64, b'W');

    clean_up_helper();
    assert!(fake_folder.sync_once());
    assert_eq!(counter.get(), 0);
    assert_eq!(counter.put(), 1);

    let mut record = file_record(&fake_folder, "parent/child/hello.odt").expect("record");
    assert!(record.is_valid());
    record.lockstate.locked = true;
    record.lockstate.lock_token = "azertyuiop".to_owned();
    record.lockstate.lock_owner_type = LockOwnerType::TokenLock as i64;
    assert!(fake_folder.sync_journal().set_file_record(&record).is_ok());
    let lock_file_path = format!("{}parent/child/.~lock.hello.odt#", fake_folder.local_path());
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_file_path)
        .is_err()
    {
        eprintln!("Failed to create lock file: {lock_file_path}");
        return;
    }
    filesystem::set_file_hidden(&lock_file_path, true);

    fake_folder.remote_modifier().remove("parent/child");
    fake_folder
        .local_modifier()
        .insert("parent/child/hello.odt", 128, b'W');

    let discovery_paths = || {
        [
            "parent",
            "parent/child",
            "parent/child/.~lock.hello.odt#",
            "parent/child/hello.odt",
        ]
        .map(str::to_owned)
    };

    clean_up_helper();
    fake_folder.sync_engine().set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        discovery_paths(),
    );
    assert!(!fake_folder.sync_once());
    assert_eq!(counter.get(), 0);
    assert_eq!(counter.put(), 1);
    fake_folder
        .sync_journal()
        .wipe_error_blacklist_category(BlacklistCategory::Normal);

    fake_folder.remote_modifier().mkdir("parent/child");
    clean_up_helper();
    fake_folder.sync_engine().set_local_discovery_options(
        LocalDiscoveryStyle::DatabaseAndFilesystem,
        discovery_paths(),
    );
    *counter.verify_lack_of_token_if_header.lock().unwrap() = true;
    assert!(fake_folder.sync_once());
    assert_eq!(counter.get(), 0);
    assert_eq!(counter.put(), 1);
    assert!(!*counter.put_with_if_header.lock().unwrap());
}
