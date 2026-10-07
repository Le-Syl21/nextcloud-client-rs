// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testchunkingng.cpp (nextcloud/desktop v34.0.5).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nc_sync::{AnotherSyncNeeded, ProgressInfo, SyncEngine, SyncOptions};
use nc_testutils::server::{chunk_move_reply, file_path_from_url, put_reply};
use nc_testutils::{FakeFolder, FakeReply, FileInfo, FileModifier, ServerState, set_temp_root};

fn header_i64(req: &nc_dav::Request, name: &str) -> i64 {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

fn is_move(req: &nc_dav::Request) -> bool {
    req.method().as_str() == "MOVE"
}

/// A new FakeFolder whose (possibly big) local files live in the target
/// directory rather than in /tmp.
fn new_fake_folder() -> FakeFolder {
    set_temp_root(env!("CARGO_TARGET_TMPDIR"));
    FakeFolder::new(FileInfo::A12_B12_C12_S12())
}

fn set_chunking_capabilities(fake_folder: &FakeFolder, with_checksums: bool) {
    if with_checksums {
        fake_folder.set_capabilities(serde_json::json!({
            "dav": { "chunking": "1.0" },
            "checksums": { "supportedTypes": ["SHA1"] } }));
    } else {
        fake_folder.set_capabilities(serde_json::json!({ "dav": { "chunking": "1.0" } }));
    }
}

/// `fakeFolder.uploadState().children.count()`.
fn upload_count(fake_folder: &FakeFolder) -> usize {
    fake_folder.upload_state().children.len()
}

/// `fakeFolder.uploadState().children.first()` (a copy).
fn first_upload(fake_folder: &FakeFolder) -> FileInfo {
    fake_folder
        .upload_state()
        .children
        .values()
        .next()
        .expect("an upload folder")
        .clone()
}

fn sum_sizes(dir: &FileInfo) -> i64 {
    dir.children.values().map(|i| i.size).sum()
}

fn remote_size(fake_folder: &FakeFolder, path: &str) -> i64 {
    fake_folder.current_remote_state().find(path).unwrap().size
}

/// Connects a `transmissionProgress` receiver.
fn on_progress(fake_folder: &mut FakeFolder, cb: impl FnMut(&ProgressInfo) + 'static) {
    fake_folder.sync_engine().callbacks().transmission_progress = Some(Box::new(cb));
}

fn disconnect_progress(fake_folder: &mut FakeFolder) {
    fake_folder.sync_engine().callbacks().transmission_progress = None;
}

/* Upload a 1/3 of a file of given size.
 * fakeFolder needs to be synchronized */
fn partial_upload(fake_folder: &mut FakeFolder, name: &str, size: i64) {
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(upload_count(fake_folder), 0); // The state should be clean

    fake_folder.local_modifier().insert(name, size, b'W');
    // Abort when the upload is at 1/3
    let size_when_abort = Rc::new(Cell::new(-1i64));
    {
        let size_when_abort = size_when_abort.clone();
        let abort = fake_folder.sync_engine().abort_handle();
        on_progress(fake_folder, move |progress| {
            if progress.completed_size() > (progress.total_size() / 3) {
                size_when_abort.set(progress.completed_size());
                abort.abort();
            }
        });
    }

    assert!(!fake_folder.sync_once()); // there should have been an error
    disconnect_progress(fake_folder);
    assert!(size_when_abort.get() > 0);
    assert!(size_when_abort.get() < size);

    assert_eq!(upload_count(fake_folder), 1); // the transfer was done with chunking
    let up_state = first_upload(fake_folder);
    assert_eq!(size_when_abort.get(), sum_sizes(&up_state));
}

// Reduce max chunk size a bit so we get more chunks
fn set_chunk_size(engine: &mut SyncEngine, size: i64) {
    let mut options = SyncOptions::default();
    // Upstream keeps the minimum file age in a static of SyncEngine that
    // FakeFolder sets once; here it is a sync option: keep it.
    options.minimum_file_age_for_upload = engine.sync_options().minimum_file_age_for_upload;
    options.set_max_chunk_size(size);
    options.set_min_chunk_size(size);
    options.initial_chunk_size = size;
    engine.set_sync_options(options);
}

/// `QScopedValueRollback<int> setHttpTimeout(AbstractNetworkJob::httpTimeout, secs)`.
struct HttpTimeoutRollback;

impl HttpTimeoutRollback {
    fn set(secs: u64) -> Self {
        nc_dav::jobs::set_http_timeout(Some(secs));
        Self
    }
}

impl Drop for HttpTimeoutRollback {
    fn drop(&mut self) {
        nc_dav::jobs::set_http_timeout(None);
    }
}

#[test]
fn test_chunk_v2_restrictions() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);

    const MAX_CHUNK_SIZE: i64 = 5 * 1000 * 1000 * 1000;
    set_chunk_size(fake_folder.sync_engine(), 10 * 1000 * 1000 * 1000);
    assert_eq!(
        fake_folder.sync_engine().sync_options().max_chunk_size(),
        MAX_CHUNK_SIZE
    );

    const MIN_CHUNK_SIZE: i64 = 5 * 1000 * 1000;
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);
    assert_eq!(
        fake_folder.sync_engine().sync_options().min_chunk_size(),
        MIN_CHUNK_SIZE
    );

    let has_destination_header = Arc::new(AtomicBool::new(false));
    {
        let has_destination_header = has_destination_header.clone();
        fake_folder.set_server_override(move |request, _| {
            if request.method() == http::Method::PUT {
                let names: Vec<_> = request.headers().keys().collect();
                eprintln!("Request headers: {names:?}");
                if request.headers().contains_key("Destination") {
                    has_destination_header.store(true, Ordering::SeqCst);
                }
            }
            None
        });
    }

    const SIZE: i64 = 1000 * 1000 * 1000; // 100 MB
    partial_upload(&mut fake_folder, "A/a0", SIZE);

    assert!(has_destination_header.load(Ordering::SeqCst));

    assert_eq!(upload_count(&fake_folder), 1);
    let upload = first_upload(&fake_folder);
    let _chunking_id = upload.name.clone();
    let first_chunk_name = upload.children.values().next().unwrap().name.clone();
    let expected_chunk_name = format!("{:05}", 1);
    assert_eq!(first_chunk_name, expected_chunk_name);
}

#[test]
fn test_file_upload() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);
    let size: i64 = 10 * 1000 * 1000; // 10 MB

    fake_folder.local_modifier().insert("A/a0", size, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(upload_count(&fake_folder), 1); // the transfer was done with chunking
    assert_eq!(remote_size(&fake_folder, "A/a0"), size);

    // Check that another upload of the same file also work.
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(upload_count(&fake_folder), 2); // the transfer was done with chunking
}

#[test]
fn test_destination_header_percent_encoding() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    let destination_header = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let destination_header = destination_header.clone();
        fake_folder.set_server_override(move |request, _| {
            let mut d = destination_header.lock().unwrap();
            if d.is_empty()
                && let Some(v) = request.headers().get("Destination")
            {
                *d = v.as_bytes().to_vec();
            }
            None
        });
    }

    let file_path = "A/SQ-0.5%BF-150/a0";
    let size: i64 = 2 * 1000 * 1000; // 2 MB

    fake_folder.local_modifier().mkdir("A/SQ-0.5%BF-150");
    fake_folder.local_modifier().insert(file_path, size, b'W');
    assert!(fake_folder.sync_once());
    let destination_header = String::from_utf8(destination_header.lock().unwrap().clone()).unwrap();
    assert!(!destination_header.is_empty());
    assert!(destination_header.contains("SQ-0.5%25BF-150"));
    assert!(destination_header.contains("/A/SQ-0.5%25BF-150/"));
    assert!(!destination_header.contains("%2F"));
    assert!(destination_header.starts_with("http://"));
}

// Test resuming when there's a confusing chunk added
#[test]
fn test_resume1() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 10 * 1000 * 1000; // 10 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);
    let chunking_id = first_upload(&fake_folder).name;
    let uploaded_size = sum_sizes(&first_upload(&fake_folder));
    assert!(uploaded_size > 2 * 1000 * 1000); // at least 2 MB

    // Add a fake chunk to make sure it gets deleted
    fake_folder
        .upload_state()
        .children
        .values_mut()
        .next()
        .unwrap()
        .insert("10000", size, b'W');

    fake_folder.set_server_override(move |request, _| {
        if request.method() == http::Method::PUT {
            // Test that we properly resuming and are not sending past data again.
            assert!(header_i64(request, "OC-Chunk-Offset") >= uploaded_size);
        } else if request.method() == http::Method::DELETE {
            assert!(request.uri().path().ends_with("/10000"));
        }
        None
    });

    assert!(fake_folder.sync_once());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size);
    // The same chunk id was re-used
    assert_eq!(upload_count(&fake_folder), 1);
    assert_eq!(first_upload(&fake_folder).name, chunking_id);
}

// Test resuming when one of the uploaded chunks got removed
#[test]
fn test_resume2() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);
    let size: i64 = 30 * 1000 * 1000; // 30 MB
    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);
    let chunking_id = first_upload(&fake_folder).name;
    let chunk_map = first_upload(&fake_folder).children;
    let uploaded_size: i64 = chunk_map.values().map(|f| f.size).sum();
    assert!(uploaded_size > 2 * 1000 * 1000); // at least 50 MB
    assert!(chunk_map.len() >= 3); // at least three chunks

    // Remove the second chunk, so all further chunks will be deleted and resent
    let first_chunk = chunk_map.values().next().unwrap().clone();
    let second_chunk = chunk_map.values().nth(1).unwrap().clone();
    let chunks_to_delete: Vec<String> = chunk_map.keys().skip(2).cloned().collect();
    fake_folder
        .upload_state()
        .children
        .values_mut()
        .next()
        .unwrap()
        .remove(&second_chunk.name);

    let deleted_paths = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let deleted_paths = deleted_paths.clone();
        let first_chunk_size = first_chunk.size;
        fake_folder.set_server_override(move |request, _| {
            if request.method() == http::Method::PUT {
                // Test that we properly resuming, not resending the first chunk
                assert!(header_i64(request, "OC-Chunk-Offset") >= first_chunk_size);
            } else if request.method() == http::Method::DELETE {
                deleted_paths
                    .lock()
                    .unwrap()
                    .push(request.uri().path().to_owned());
            }
            None
        });
    }

    assert!(fake_folder.sync_once());

    for to_delete in &chunks_to_delete {
        let was_deleted = deleted_paths
            .lock()
            .unwrap()
            .iter()
            .any(|deleted| &deleted[deleted.rfind('/').unwrap() + 1..] == to_delete);
        assert!(was_deleted);
    }

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size);
    // The same chunk id was re-used
    assert_eq!(upload_count(&fake_folder), 1);
    assert_eq!(first_upload(&fake_folder).name, chunking_id);
}

// Test resuming when all chunks are already present
#[test]
fn test_resume3() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 30 * 1000 * 1000; // 30 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);
    let chunking_id = first_upload(&fake_folder).name;
    let chunk_map = first_upload(&fake_folder).children;
    let uploaded_size: i64 = chunk_map.values().map(|f| f.size).sum();
    assert!(uploaded_size > 5 * 1000 * 1000); // at least 5 MB

    // Add a chunk that makes the file completely uploaded
    let test_chunk_name_num = chunk_map.len() + 1; // Chunk nums start at 1 with Chunk V2, so size() == last num, add 1
    let test_chunk_name = format!("{test_chunk_name_num:05}");
    let test_chunk_size = size - uploaded_size;
    fake_folder
        .upload_state()
        .children
        .values_mut()
        .next()
        .unwrap()
        .insert(&test_chunk_name, test_chunk_size, b'W');

    let saw_put = Arc::new(AtomicBool::new(false));
    let saw_delete = Arc::new(AtomicBool::new(false));
    let saw_move = Arc::new(AtomicBool::new(false));
    {
        let (saw_put, saw_delete, saw_move) =
            (saw_put.clone(), saw_delete.clone(), saw_move.clone());
        fake_folder.set_server_override(move |request, _| {
            if request.method() == http::Method::PUT {
                saw_put.store(true, Ordering::SeqCst);
            } else if request.method() == http::Method::DELETE {
                saw_delete.store(true, Ordering::SeqCst);
            } else if is_move(request) {
                saw_move.store(true, Ordering::SeqCst);
            }
            None
        });
    }

    assert!(fake_folder.sync_once());
    assert!(saw_move.load(Ordering::SeqCst));
    assert!(!saw_put.load(Ordering::SeqCst));
    assert!(!saw_delete.load(Ordering::SeqCst));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size);
    // The same chunk id was re-used
    assert_eq!(upload_count(&fake_folder), 1);
    assert_eq!(first_upload(&fake_folder).name, chunking_id);
}

// Test resuming (or rather not resuming!) for the error case of the sum of
// chunk sizes being larger than the file size
#[test]
fn test_resume4() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);

    const SIZE: i64 = 30 * 1000 * 1000; // 30 MB
    const CHUNK_SIZE: i64 = 5 * 1000 * 1000; // 5 MB
    set_chunk_size(fake_folder.sync_engine(), CHUNK_SIZE);

    partial_upload(&mut fake_folder, "A/a0", SIZE);
    assert_eq!(upload_count(&fake_folder), 1);

    let chunking_id = first_upload(&fake_folder).name;
    let chunk_map = first_upload(&fake_folder).children;

    let uploaded_size: i64 = chunk_map.values().map(|f| f.size).sum();
    assert!(uploaded_size > 5 * 1000 * 1000); // at least 5 MB

    // Add a chunk that makes the file more than completely uploaded
    let test_chunk_name_num = chunk_map.len() + 1; // Chunk nums start at 1 with Chunk V2, so size() == last num, add 1
    let test_chunk_name = format!("{test_chunk_name_num:05}");
    let test_chunk_size = SIZE - uploaded_size + 100;
    fake_folder
        .upload_state()
        .children
        .values_mut()
        .next()
        .unwrap()
        .insert(&test_chunk_name, test_chunk_size, b'W');

    assert!(fake_folder.sync_once());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), SIZE);
    // Used a new transfer id but wiped the old one
    assert_eq!(upload_count(&fake_folder), 1);
    assert_ne!(first_upload(&fake_folder).name, chunking_id);
}

/// `new DelayedReply<FakeChunkMoveReply>(delay, uploadState, remoteModifier, ...)`:
/// the MOVE is performed right away, the reply comes after `delay`.
fn delayed_chunk_move(
    delay: Duration,
    request: &nc_dav::Request,
    state: &mut ServerState,
) -> FakeReply {
    let file_name = file_path_from_url(request.uri()).unwrap_or_default();
    let ServerState {
        upload_root,
        remote_root,
        ..
    } = state;
    FakeReply::Delayed(
        delay,
        chunk_move_reply(upload_root, remote_root, request, &file_name),
    )
}

// Check what happens when we abort during the final MOVE and the
// the final MOVE takes longer than the abort-delay
#[test]
fn test_late_abort_hard() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, true);
    let size: i64 = 15 * 1000 * 1000; // 15 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    // Make the MOVE never reply, but trigger a client-abort and apply the change remotely
    let move_checksum_header = Arc::new(Mutex::new(Vec::<u8>::new()));
    let n_get = Arc::new(AtomicI32::new(0));
    let response_delay = Duration::from_millis(100000); // bigger than abort-wait timeout
    {
        let move_checksum_header = move_checksum_header.clone();
        let n_get = n_get.clone();
        let abort_timer = fake_folder.abort_timer();
        fake_folder.set_server_override(move |request, state| {
            if is_move(request) {
                abort_timer.single_shot(Duration::from_millis(50));
                *move_checksum_header.lock().unwrap() = request
                    .headers()
                    .get("OC-Checksum")
                    .map(|v| v.as_bytes().to_vec())
                    .unwrap_or_default();
                return Some(delayed_chunk_move(response_delay, request, state));
            } else if request.method() == http::Method::GET {
                n_get.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    }

    // Test 1: NEW file aborted
    fake_folder.local_modifier().insert("A/a0", size, b'W');
    assert!(!fake_folder.sync_once()); // error: abort!

    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Test 2: modified file upload aborted
    n_get.store(0, Ordering::SeqCst);
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(!fake_folder.sync_once()); // error: abort!

    // An EVAL/EVAL conflict is also UPDATE_METADATA when there's no checksums
    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Test 3: modified file upload aborted, with good checksums
    n_get.store(0, Ordering::SeqCst);
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(!fake_folder.sync_once()); // error: abort!

    // Set the remote checksum -- the test setup doesn't do it automatically
    let checksum = move_checksum_header.lock().unwrap().clone();
    assert!(!checksum.is_empty());
    fake_folder
        .remote_modifier()
        .find_mut("A/a0")
        .unwrap()
        .checksums = String::from_utf8(checksum).unwrap();

    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 0); // no new download, just a metadata update!
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Test 4: New file, that gets deleted locally before the next sync
    n_get.store(0, Ordering::SeqCst);
    fake_folder.local_modifier().insert("A/a3", size, b'W');
    assert!(!fake_folder.sync_once()); // error: abort!
    fake_folder.local_modifier().remove("A/a3");

    // bug: in this case we must expect a re-download of A/A3
    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 1);
    assert!(fake_folder.current_local_state().find("A/a3").is_some());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

// Check what happens when we abort during the final MOVE and the
// the final MOVE is short enough for the abort-delay to help
#[test]
fn test_late_abort_recoverable() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, true);
    let size: i64 = 15 * 1000 * 1000; // 15 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    // Make the MOVE never reply, but trigger a client-abort and apply the change remotely
    let response_delay = Duration::from_millis(200); // smaller than abort-wait timeout
    {
        let abort_timer = fake_folder.abort_timer();
        fake_folder.set_server_override(move |request, state| {
            if is_move(request) {
                abort_timer.single_shot(Duration::from_millis(50));
                return Some(delayed_chunk_move(response_delay, request, state));
            }
            None
        });
    }

    // Test 1: NEW file aborted
    fake_folder.local_modifier().insert("A/a0", size, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Test 2: modified file upload aborted
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

// We modify the file locally after it has been partially uploaded
#[test]
fn test_remove_stale1() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 10 * 1000 * 1000; // 10 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);
    let chunking_id = first_upload(&fake_folder).name;

    fake_folder.local_modifier().set_contents("A/a0", b'B');
    fake_folder.local_modifier().append_byte("A/a0");

    assert!(fake_folder.sync_once());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size + 1);
    // A different chunk id was used, and the previous one is removed
    assert_eq!(upload_count(&fake_folder), 1);
    assert_ne!(first_upload(&fake_folder).name, chunking_id);
}

// We remove the file locally after it has been partially uploaded
#[test]
fn test_remove_stale2() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 10 * 1000 * 1000; // 10 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);

    fake_folder.local_modifier().remove("A/a0");

    assert!(fake_folder.sync_once());
    assert_eq!(upload_count(&fake_folder), 0);
}

#[test]
fn test_create_conflict_while_syncing() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 10 * 1000 * 1000; // 10 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    // Put a file on the server and download it.
    fake_folder.remote_modifier().insert("A/a0", size, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Modify the file locally and start the upload
    fake_folder.local_modifier().set_contents("A/a0", b'B');
    fake_folder.local_modifier().append_byte("A/a0");

    // But in the middle of the sync, modify the file on the server
    {
        let server = fake_folder.server().clone();
        let connected = Cell::new(true);
        on_progress(&mut fake_folder, move |progress| {
            if connected.get() && progress.completed_size() > (progress.total_size() / 2) {
                server.state().remote_root.set_contents("A/a0", b'C');
                connected.set(false); // QObject::disconnect(con)
            }
        });
    }

    assert!(!fake_folder.sync_once());
    // There was a precondition failed error, this means wen need to sync again
    assert_eq!(
        fake_folder.sync_engine().is_another_sync_needed(),
        AnotherSyncNeeded::ImmediateFollowUp
    );

    assert_eq!(upload_count(&fake_folder), 1); // We did not clean the chunks at this point

    // Now we will download the server file and create a conflict
    assert!(fake_folder.sync_once());
    let local_state = fake_folder.current_local_state();

    // A0 is the one from the server
    assert_eq!(local_state.find("A/a0").unwrap().size, size);
    assert_eq!(local_state.find("A/a0").unwrap().content_char, b'C');

    // There is a conflict file with our version
    let state_a_children = &local_state.find("A").unwrap().children;
    let it = state_a_children
        .values()
        .find(|fi| fi.name.starts_with("a0 (conflicted copy"));
    assert!(it.is_some());
    let it = it.unwrap();
    assert_eq!(it.content_char, b'B');
    assert_eq!(it.size, size + 1);

    // Remove the conflict file so the comparison works!
    fake_folder
        .local_modifier()
        .remove(&format!("A/{}", it.name));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    assert_eq!(upload_count(&fake_folder), 0); // The last sync cleaned the chunks
}

#[test]
fn test_modify_local_file_while_uploading() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 10 * 1000 * 1000; // 10 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    fake_folder.local_modifier().insert("A/a0", size, b'W');

    // middle of the sync, modify the file
    {
        let mut local = fake_folder.local_modifier().clone();
        let connected = Cell::new(true);
        on_progress(&mut fake_folder, move |progress| {
            if connected.get() && progress.completed_size() > (progress.total_size() / 2) {
                local.set_contents("A/a0", b'B');
                local.append_byte("A/a0");
                connected.set(false); // QObject::disconnect(con)
            }
        });
    }

    assert!(!fake_folder.sync_once());

    // There should be a followup sync
    assert_eq!(
        fake_folder.sync_engine().is_another_sync_needed(),
        AnotherSyncNeeded::ImmediateFollowUp
    );

    assert_eq!(upload_count(&fake_folder), 1); // We did not clean the chunks at this point
    let chunking_id = first_upload(&fake_folder).name;

    // Now we make a new sync which should upload the file for good.
    assert!(fake_folder.sync_once());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size + 1);

    // A different chunk id was used, and the previous one is removed
    assert_eq!(upload_count(&fake_folder), 1);
    assert_ne!(first_upload(&fake_folder).name, chunking_id);
}

#[test]
fn test_resume_server_deleted_chunks() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 30 * 1000 * 1000; // 30 MB
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);
    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);
    let chunking_id = first_upload(&fake_folder).name;

    // Delete the chunks on the server
    fake_folder.upload_state().children.clear();
    assert!(fake_folder.sync_once());

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size);

    // A different chunk id was used
    assert_eq!(upload_count(&fake_folder), 1);
    assert_ne!(first_upload(&fake_folder).name, chunking_id);
}

// Check what happens when the connection is dropped on the PUT (non-chunking) or MOVE (chunking)
// for on the issue #5106
fn connection_dropped_before_etag_recieved_row(chunking: bool) {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, true);
    let size: i64 = if chunking { 1000 * 1000 } else { 300 };
    set_chunk_size(fake_folder.sync_engine(), 300 * 1000);

    // Make the MOVE never reply, but trigger a client-abort and apply the change remotely
    let checksum_header = Arc::new(Mutex::new(Vec::<u8>::new()));
    let n_get = Arc::new(AtomicI32::new(0));
    let _set_http_timeout = HttpTimeoutRollback::set(1);
    // much bigger than http timeout (so a timeout will occur)
    let response_delay = Duration::from_secs(1000 * 1000);
    // This will perform the operation on the server, but the reply will not come to the client
    {
        let checksum_header = checksum_header.clone();
        let n_get = n_get.clone();
        fake_folder.set_server_override(move |request, state| {
            if !chunking {
                assert!(
                    !request.uri().path().contains("/uploads/"),
                    "Should not touch uploads endpoint when not chunking"
                );
            }
            let oc_checksum = || {
                request
                    .headers()
                    .get("OC-Checksum")
                    .map(|v| v.as_bytes().to_vec())
                    .unwrap_or_default()
            };
            if !chunking && request.method() == http::Method::PUT {
                *checksum_header.lock().unwrap() = oc_checksum();
                let file_name = file_path_from_url(request.uri()).unwrap_or_default();
                return Some(FakeReply::Delayed(
                    response_delay,
                    put_reply(&mut state.remote_root, request, &file_name),
                ));
            } else if chunking && is_move(request) {
                *checksum_header.lock().unwrap() = oc_checksum();
                return Some(delayed_chunk_move(response_delay, request, state));
            } else if request.method() == http::Method::GET {
                n_get.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    }
    let set_remote_checksum = |fake_folder: &FakeFolder| {
        let checksum = String::from_utf8(checksum_header.lock().unwrap().clone()).unwrap();
        fake_folder
            .remote_modifier()
            .find_mut("A/a0")
            .unwrap()
            .checksums = checksum;
    };

    // Test 1: a NEW file
    fake_folder.local_modifier().insert("A/a0", size, b'W');
    assert!(!fake_folder.sync_once()); // timeout!
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    ); // but the upload succeeded
    assert!(!checksum_header.lock().unwrap().is_empty());
    set_remote_checksum(&fake_folder); // The test system don't do that automatically
    // Should be resolved properly
    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 0);
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Test 2: Modify the file further
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(!fake_folder.sync_once()); // timeout!
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    ); // but the upload succeeded
    set_remote_checksum(&fake_folder);
    // modify again, should not cause conflict
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(!fake_folder.sync_once()); // now it's trying to upload the modified file
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    set_remote_checksum(&fake_folder);
    assert!(fake_folder.sync_once());
    assert_eq!(n_get.load(Ordering::SeqCst), 0);
}

#[test]
fn connection_dropped_before_etag_recieved() {
    // _data row "big file"
    connection_dropped_before_etag_recieved_row(true);
    // _data row "small file"
    connection_dropped_before_etag_recieved_row(false);
}

#[test]
fn test_percent_encoding() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);
    let size: i64 = 5 * 1000 * 1000;
    set_chunk_size(fake_folder.sync_engine(), 1000 * 1000);

    fake_folder
        .local_modifier()
        .insert("A/file % \u{20ac}", size, b'W');
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );

    // Only the second upload contains an "If" header
    fake_folder
        .local_modifier()
        .append_byte("A/file % \u{20ac}");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

// Test uploading large files (2.5GiB)
#[test]
fn test_very_big_files() {
    let mut fake_folder = new_fake_folder();
    set_chunking_capabilities(&fake_folder, false);

    // Dynamic chunk sizing tries to go for biggest chunks possible depending on upload speed.
    // In the tests this is immediate, so we need to give a file larger than the max chunk size
    // and cap the max chunk size a bit
    let mut opts = fake_folder.sync_engine().sync_options().clone();
    opts.set_max_chunk_size(500 * 1000 * 1000); // 500MB
    fake_folder.sync_engine().set_sync_options(opts);
    let size: i64 = (2.5 * 1024.0 * 1024.0 * 1024.0) as i64; // 2.5 GiB

    // Partial upload of big files
    partial_upload(&mut fake_folder, "A/a0", size);
    assert_eq!(upload_count(&fake_folder), 1);
    let chunking_id = first_upload(&fake_folder).name;

    // Now resume
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size);

    // The same chunk id was re-used
    assert_eq!(upload_count(&fake_folder), 1);
    assert_eq!(first_upload(&fake_folder).name, chunking_id);

    // Upload another file again, this time without interruption
    fake_folder.local_modifier().append_byte("A/a0");
    assert!(fake_folder.sync_once());
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(remote_size(&fake_folder, "A/a0"), size + 1);
}
