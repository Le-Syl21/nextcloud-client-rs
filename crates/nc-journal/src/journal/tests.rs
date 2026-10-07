// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream `test/testsyncjournaldb.cpp` (nextcloud/desktop v34.0.5),
// one Rust test per upstream test function, same names in snake_case.
//
// Upstream shares one database between all test functions (run in order);
// here every test gets its own fresh database. No upstream assertion depends
// on state left by a previous test function.

use super::*;
use crate::csync::ItemType;
use crate::pinstate::PinState;
use crate::remote_permissions::RemotePermissions;

struct Fixture {
    _dir: tempfile::TempDir,
    db: SyncJournalDb,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = SyncJournalDb::new(dir.path().join("sync.db"));
    Fixture { _dir: dir, db }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn test_file_record() {
    let f = fixture();
    let db = &f.db;
    let record = db.get_file_record(b"nonexistent").unwrap();
    assert!(record.is_none());

    let mut record = SyncJournalFileRecord {
        path: b"foo".to_vec(),
        // Use a value that exceeds uint32 and isn't representable by the
        // signed int being cast to uint64 either (like uint64::max would be)
        inode: u64::from(u32::MAX) + 12,
        modtime: now_secs(),
        item_type: ItemType::Directory,
        etag: b"789789".to_vec(),
        file_id: b"abcd".to_vec(),
        remote_perm: RemotePermissions::from_db_value(b"RW"),
        file_size: 213089055,
        checksum_header: b"MD5:mychecksum".to_vec(),
        ..Default::default()
    };
    db.set_file_record(&record).unwrap();

    let stored = db.get_file_record(b"foo").unwrap().unwrap();
    assert!(stored == record);

    // Update checksum
    record.checksum_header = b"Adler32:newchecksum".to_vec();
    db.update_file_record_checksum("foo", b"newchecksum", b"Adler32")
        .unwrap();
    let stored = db.get_file_record(b"foo").unwrap().unwrap();
    assert!(stored == record);

    // Update metadata
    record.modtime = now_secs() + 86400;
    // try a value that only fits uint64, not int64
    record.inode = u64::MAX - u64::from(u32::MAX) - 1;
    record.item_type = ItemType::File;
    record.etag = b"789FFF".to_vec();
    record.file_id = b"efg".to_vec();
    record.remote_perm = RemotePermissions::from_db_value(b"NV");
    record.file_size = 289055;
    db.set_file_record(&record).unwrap();
    let stored = db.get_file_record(b"foo").unwrap().unwrap();
    assert!(stored == record);

    db.delete_file_record("foo", false).unwrap();
    let record = db.get_file_record(b"foo").unwrap();
    assert!(record.is_none());
}

#[test]
fn test_folder_quota() {
    let f = fixture();
    let db = &f.db;
    let big_folder_record = b"bigfolder";
    let mut record = SyncJournalFileRecord {
        path: big_folder_record.to_vec(),
        inode: u64::from(u32::MAX) + 12,
        modtime: now_secs(),
        item_type: ItemType::Directory,
        etag: b"123123".to_vec(),
        file_id: b"abcd".to_vec(),
        file_size: 213089999,
        ..Default::default()
    };
    db.set_file_record(&record).unwrap();

    let stored = db.get_file_record(big_folder_record).unwrap().unwrap();
    assert!(stored == record);
    // default values
    assert_eq!(stored.folder_quota.bytes_available, -1);
    assert_eq!(stored.folder_quota.bytes_used, -1);

    record.folder_quota.bytes_used = 100;
    record.folder_quota.bytes_available = 5000;
    db.set_file_record(&record).unwrap();
    let stored = db.get_file_record(big_folder_record).unwrap().unwrap();
    assert!(stored == record);
    assert_eq!(stored.folder_quota.bytes_available, 5000);
    assert_eq!(stored.folder_quota.bytes_used, 100);
}

#[test]
fn test_folder_migration() {
    let f = fixture();
    assert!(f.db.open());
    let mut inner = f.db.lock();
    let quota_bytes_used = "quotaBytesUsed";
    let quota_bytes_available = "quotaBytesAvailable";
    let columns_before_removal = inner.table_columns("metadata");
    assert!(columns_before_removal.iter().any(|c| c == quota_bytes_used));
    assert!(
        columns_before_removal
            .iter()
            .any(|c| c == quota_bytes_available)
    );
    assert!(inner.remove_column(quota_bytes_available));
    assert!(inner.remove_column(quota_bytes_used));
    assert!(inner.update_metadata_table_structure());
    let columns_after_connect = inner.table_columns("metadata");
    assert!(columns_after_connect.iter().any(|c| c == quota_bytes_used));
    assert!(
        columns_after_connect
            .iter()
            .any(|c| c == quota_bytes_available)
    );
    assert!(inner.has_default_value(quota_bytes_used));
    assert!(inner.has_default_value(quota_bytes_available));
}

#[test]
fn test_file_record_checksum() {
    let f = fixture();
    let db = &f.db;
    // Try with and without a checksum
    {
        let record = SyncJournalFileRecord {
            path: b"foo-checksum".to_vec(),
            remote_perm: RemotePermissions::from_db_value(b" "),
            checksum_header: b"MD5:mychecksum".to_vec(),
            modtime: now_secs(),
            ..Default::default()
        };
        db.set_file_record(&record).unwrap();

        let stored = db.get_file_record(b"foo-checksum").unwrap().unwrap();
        assert!(stored.path == record.path);
        assert!(stored.remote_perm == record.remote_perm);
        assert!(stored.checksum_header == record.checksum_header);
        // Attention: compare time_t types here, as QDateTime seem to maintain
        // milliseconds internally, which disappear in sqlite. Go for full seconds here.
        assert!(stored.modtime == record.modtime);
        assert!(stored == record);
    }
    {
        let record = SyncJournalFileRecord {
            path: b"foo-nochecksum".to_vec(),
            remote_perm: RemotePermissions::from_db_value(b"RW"),
            modtime: now_secs(),
            ..Default::default()
        };
        db.set_file_record(&record).unwrap();
        let stored = db.get_file_record(b"foo-nochecksum").unwrap().unwrap();
        assert!(stored == record);
    }
}

#[test]
fn test_download_info() {
    let f = fixture();
    let db = &f.db;
    let mut record = db.get_download_info("nonexistent");
    assert!(!record.valid);

    record.error_count = 5;
    record.etag = b"ABCDEF".to_vec();
    record.valid = true;
    record.tmpfile = "/tmp/foo".to_owned();
    db.set_download_info("foo", &record);

    let stored = db.get_download_info("foo");
    assert!(stored == record);

    db.set_download_info("foo", &DownloadInfo::default());
    let wiped = db.get_download_info("foo");
    assert!(!wiped.valid);
}

#[test]
fn test_upload_info() {
    let f = fixture();
    let db = &f.db;
    let mut record = db.get_upload_info("nonexistent");
    assert!(!record.valid);

    record.error_count = 5;
    record.chunk_upload_v1 = 12;
    record.transferid = 812974891;
    record.size = 12894789147;
    record.modtime = now_secs();
    record.valid = true;
    db.set_upload_info("foo", &record);

    let stored = db.get_upload_info("foo");
    assert!(stored == record);

    db.set_upload_info("foo", &UploadInfo::default());
    let wiped = db.get_upload_info("foo");
    assert!(!wiped.valid);
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn test_numeric_id() {
    let mut record = SyncJournalFileRecord::default();

    // Typical 8-digit padded id
    record.file_id = b"00000001abcd".to_vec();
    assert_eq!(record.numeric_file_id(), b"00000001");

    // When the numeric id overflows the 8-digit boundary
    record.file_id = b"123456789ocidblaabcd".to_vec();
    assert_eq!(record.numeric_file_id(), b"123456789");
}

#[test]
fn test_conflict_record() {
    let f = fixture();
    let db = &f.db;
    let record = ConflictRecord {
        path: b"abc".to_vec(),
        base_file_id: b"def".to_vec(),
        base_modtime: 1234,
        base_etag: b"ghi".to_vec(),
        ..Default::default()
    };

    assert!(!db.conflict_record(&record.path).is_valid());

    db.set_conflict_record(&record);
    let new_record = db.conflict_record(&record.path);
    assert!(new_record.is_valid());
    assert_eq!(new_record.path, record.path);
    assert_eq!(new_record.base_file_id, record.base_file_id);
    assert_eq!(new_record.base_modtime, record.base_modtime);
    assert_eq!(new_record.base_etag, record.base_etag);

    db.delete_conflict_record(&record.path);
    assert!(!db.conflict_record(&record.path).is_valid());
}

#[test]
fn test_avoid_read_from_db_on_next_sync() {
    let f = fixture();
    let db = &f.db;
    let invalid_etag = b"_invalid_".to_vec();
    let mut initial_etag = b"etag".to_vec();
    let make_entry = |path: &str, ty: ItemType, etag: &[u8]| {
        let record = SyncJournalFileRecord {
            modtime: now_secs(),
            path: path.as_bytes().to_vec(),
            item_type: ty,
            etag: etag.to_vec(),
            remote_perm: RemotePermissions::from_db_value(b"RW"),
            ..Default::default()
        };
        db.set_file_record(&record).unwrap();
    };
    let get_etag = |path: &str| {
        db.get_file_record(path.as_bytes())
            .unwrap()
            .map(|r| r.etag)
            .unwrap_or_default()
    };

    make_entry("foodir", ItemType::Directory, &initial_etag);
    make_entry("otherdir", ItemType::Directory, &initial_etag);
    make_entry("foo%", ItemType::Directory, &initial_etag); // wildcards don't apply
    make_entry("foodi_", ItemType::Directory, &initial_etag); // wildcards don't apply
    make_entry("foodir/file", ItemType::File, &initial_etag);
    make_entry("foodir/subdir", ItemType::Directory, &initial_etag);
    make_entry("foodir/subdir/file", ItemType::File, &initial_etag);
    make_entry("foodir/otherdir", ItemType::Directory, &initial_etag);
    make_entry("fo", ItemType::Directory, &initial_etag); // prefix, but does not match
    make_entry("foodir/sub", ItemType::Directory, &initial_etag); // prefix, but does not match
    make_entry(
        "foodir/subdir/subsubdir",
        ItemType::Directory,
        &initial_etag,
    );
    make_entry(
        "foodir/subdir/subsubdir/file",
        ItemType::File,
        &initial_etag,
    );
    make_entry("foodir/subdir/otherdir", ItemType::Directory, &initial_etag);

    db.schedule_path_for_remote_discovery(b"foodir/subdir");

    // Direct effects of parent directories being set to _invalid_
    assert_eq!(get_etag("foodir"), invalid_etag);
    assert_eq!(get_etag("foodir/subdir"), invalid_etag);
    assert_eq!(get_etag("foodir/subdir/subsubdir"), initial_etag);

    assert_eq!(get_etag("foodir/file"), initial_etag);
    assert_eq!(get_etag("foodir/subdir/file"), initial_etag);
    assert_eq!(get_etag("foodir/subdir/subsubdir/file"), initial_etag);

    assert_eq!(get_etag("fo"), initial_etag);
    assert_eq!(get_etag("foo%"), initial_etag);
    assert_eq!(get_etag("foodi_"), initial_etag);
    assert_eq!(get_etag("otherdir"), initial_etag);
    assert_eq!(get_etag("foodir/otherdir"), initial_etag);
    assert_eq!(get_etag("foodir/sub"), initial_etag);
    assert_eq!(get_etag("foodir/subdir/otherdir"), initial_etag);

    // Indirect effects: setFileRecord() calls filter etags
    initial_etag = b"etag2".to_vec();

    make_entry("foodir", ItemType::Directory, &initial_etag);
    assert_eq!(get_etag("foodir"), invalid_etag);
    make_entry("foodir/subdir", ItemType::Directory, &initial_etag);
    assert_eq!(get_etag("foodir/subdir"), invalid_etag);
    make_entry(
        "foodir/subdir/subsubdir",
        ItemType::Directory,
        &initial_etag,
    );
    assert_eq!(get_etag("foodir/subdir/subsubdir"), initial_etag);
    make_entry("fo", ItemType::Directory, &initial_etag);
    assert_eq!(get_etag("fo"), initial_etag);
    make_entry("foodir/sub", ItemType::Directory, &initial_etag);
    assert_eq!(get_etag("foodir/sub"), initial_etag);
}

#[test]
fn test_recursive_delete() {
    let f = fixture();
    let db = &f.db;
    let make_entry = |path: &str| {
        let record = SyncJournalFileRecord {
            path: path.as_bytes().to_vec(),
            remote_perm: RemotePermissions::from_db_value(b"RW"),
            modtime: now_secs(),
            ..Default::default()
        };
        db.set_file_record(&record).unwrap();
    };

    let mut elements = vec![
        "foo",
        "foo/file",
        "bar",
        "moo",
        "moo/file",
        "foo%bar",
        "foo bla bar/file",
        "fo_",
        "fo_/file",
    ];
    for elem in &elements {
        make_entry(elem);
    }

    let check_elements = |elements: &[&str]| {
        let mut ok = true;
        for elem in elements {
            match db.get_file_record(elem.as_bytes()) {
                Ok(Some(r)) if r.is_valid() => {}
                _ => {
                    eprintln!("Missing record: {elem}");
                    ok = false;
                }
            }
        }
        ok
    };

    db.delete_file_record("moo", true).unwrap();
    elements.retain(|e| *e != "moo" && *e != "moo/file");
    assert!(check_elements(&elements));

    db.delete_file_record("fo_", true).unwrap();
    elements.retain(|e| *e != "fo_" && *e != "fo_/file");
    assert!(check_elements(&elements));

    db.delete_file_record("foo%bar", true).unwrap();
    elements.retain(|e| *e != "foo%bar");
    assert!(check_elements(&elements));
}

#[test]
fn test_pin_state() {
    let f = fixture();
    let db = &f.db;
    let make = |path: &str, state: PinState| {
        db.internal_pin_states()
            .set_for_path(path.as_bytes(), state);
        let pin_state = db.internal_pin_states().raw_for_path(path.as_bytes());
        assert!(pin_state.is_some());
        assert_eq!(pin_state.unwrap(), state);
    };
    let get = |path: &str| -> PinState {
        db.internal_pin_states()
            .effective_for_path(path.as_bytes())
            .expect("couldn't read pin state")
    };
    let get_recursive = |path: &str| -> PinState {
        db.internal_pin_states()
            .effective_for_path_recursive(path.as_bytes())
            .expect("couldn't read pin state")
    };
    let get_raw = |path: &str| -> PinState {
        db.internal_pin_states()
            .raw_for_path(path.as_bytes())
            .expect("couldn't read pin state")
    };

    db.internal_pin_states().wipe_for_path_and_below(b"");
    let list = db.internal_pin_states().raw_list();
    assert_eq!(list.unwrap().len(), 0);

    // Make a thrice-nested setup
    make("", PinState::AlwaysLocal);
    make("local", PinState::AlwaysLocal);
    make("online", PinState::OnlineOnly);
    make("inherit", PinState::Inherited);
    for base in ["local/", "online/", "inherit/"] {
        make(&format!("{base}inherit"), PinState::Inherited);
        make(&format!("{base}local"), PinState::AlwaysLocal);
        make(&format!("{base}online"), PinState::OnlineOnly);

        for base2 in ["local/", "online/", "inherit/"] {
            make(&format!("{base}{base2}inherit"), PinState::Inherited);
            make(&format!("{base}{base2}local"), PinState::AlwaysLocal);
            make(&format!("{base}{base2}online"), PinState::OnlineOnly);
        }
    }

    let list = db.internal_pin_states().raw_list();
    assert_eq!(list.unwrap().len(), 4 + 9 + 27);

    // Baseline direct checks (the fallback for unset root pinstate is AlwaysLocal)
    assert_eq!(get(""), PinState::AlwaysLocal);
    assert_eq!(get("local"), PinState::AlwaysLocal);
    assert_eq!(get("online"), PinState::OnlineOnly);
    assert_eq!(get("inherit"), PinState::AlwaysLocal);
    assert_eq!(get("nonexistent"), PinState::AlwaysLocal);
    assert_eq!(get("online/local"), PinState::AlwaysLocal);
    assert_eq!(get("local/online"), PinState::OnlineOnly);
    assert_eq!(get("inherit/local"), PinState::AlwaysLocal);
    assert_eq!(get("inherit/online"), PinState::OnlineOnly);
    assert_eq!(get("inherit/inherit"), PinState::AlwaysLocal);
    assert_eq!(get("inherit/nonexistent"), PinState::AlwaysLocal);

    // Inheriting checks, level 1
    assert_eq!(get("local/inherit"), PinState::AlwaysLocal);
    assert_eq!(get("local/nonexistent"), PinState::AlwaysLocal);
    assert_eq!(get("online/inherit"), PinState::OnlineOnly);
    assert_eq!(get("online/nonexistent"), PinState::OnlineOnly);

    // Inheriting checks, level 2
    assert_eq!(get("local/inherit/inherit"), PinState::AlwaysLocal);
    assert_eq!(get("local/local/inherit"), PinState::AlwaysLocal);
    assert_eq!(get("local/local/nonexistent"), PinState::AlwaysLocal);
    assert_eq!(get("local/online/inherit"), PinState::OnlineOnly);
    assert_eq!(get("local/online/nonexistent"), PinState::OnlineOnly);
    assert_eq!(get("online/inherit/inherit"), PinState::OnlineOnly);
    assert_eq!(get("online/local/inherit"), PinState::AlwaysLocal);
    assert_eq!(get("online/local/nonexistent"), PinState::AlwaysLocal);
    assert_eq!(get("online/online/inherit"), PinState::OnlineOnly);
    assert_eq!(get("online/online/nonexistent"), PinState::OnlineOnly);

    // Spot check the recursive variant
    assert_eq!(get_recursive(""), PinState::Inherited);
    assert_eq!(get_recursive("local"), PinState::Inherited);
    assert_eq!(get_recursive("online"), PinState::Inherited);
    assert_eq!(get_recursive("inherit"), PinState::Inherited);
    assert_eq!(get_recursive("online/local"), PinState::Inherited);
    assert_eq!(get_recursive("online/local/inherit"), PinState::AlwaysLocal);
    assert_eq!(
        get_recursive("inherit/inherit/inherit"),
        PinState::AlwaysLocal
    );
    assert_eq!(
        get_recursive("inherit/online/inherit"),
        PinState::OnlineOnly
    );
    assert_eq!(get_recursive("inherit/online/local"), PinState::AlwaysLocal);
    make("local/local/local/local", PinState::AlwaysLocal);
    assert_eq!(get_recursive("local/local/local"), PinState::AlwaysLocal);
    assert_eq!(
        get_recursive("local/local/local/local"),
        PinState::AlwaysLocal
    );

    // Check changing the root pin state
    make("", PinState::OnlineOnly);
    assert_eq!(get("local"), PinState::AlwaysLocal);
    assert_eq!(get("online"), PinState::OnlineOnly);
    assert_eq!(get("inherit"), PinState::OnlineOnly);
    assert_eq!(get("nonexistent"), PinState::OnlineOnly);
    make("", PinState::AlwaysLocal);
    assert_eq!(get("local"), PinState::AlwaysLocal);
    assert_eq!(get("online"), PinState::OnlineOnly);
    assert_eq!(get("inherit"), PinState::AlwaysLocal);
    assert_eq!(get("nonexistent"), PinState::AlwaysLocal);

    // Wiping
    assert_eq!(get_raw("local/local"), PinState::AlwaysLocal);
    db.internal_pin_states()
        .wipe_for_path_and_below(b"local/local");
    assert_eq!(get_raw("local"), PinState::AlwaysLocal);
    assert_eq!(get_raw("local/local"), PinState::Inherited);
    assert_eq!(get_raw("local/local/local"), PinState::Inherited);
    assert_eq!(get_raw("local/local/online"), PinState::Inherited);
    let list = db.internal_pin_states().raw_list();
    assert_eq!(list.unwrap().len(), 4 + 9 + 27 - 4);

    // Wiping everything
    db.internal_pin_states().wipe_for_path_and_below(b"");
    assert_eq!(get_raw(""), PinState::Inherited);
    assert_eq!(get_raw("local"), PinState::Inherited);
    assert_eq!(get_raw("online"), PinState::Inherited);
    let list = db.internal_pin_states().raw_list();
    assert_eq!(list.unwrap().len(), 0);
}

#[test]
fn test_has_file_ids() {
    let f = fixture();
    let db = &f.db;
    let mut all_file_ids: Vec<i64> = Vec::new();
    let mut make_entry = |file_id: i64| {
        // u"%1oc123xyz987e"_s.arg(fileId, 8, 10, '0'_L1): zero padding to a
        // field width of 8, after the sign (the test relies on "-0000009").
        let number = format!("{file_id:08}");
        let record = SyncJournalFileRecord {
            file_id: format!("{number}oc123xyz987e").into_bytes(),
            modtime: now_secs(),
            path: format!("item{file_id}").into_bytes(),
            item_type: ItemType::File,
            etag: b"etag".to_vec(),
            ..Default::default()
        };
        db.set_file_record(&record).unwrap();
        all_file_ids.push(file_id);
    };

    // generate some test data: -9, 0..32, int32_max..(int32_max + 32), (int64_max - 32)..int64_max
    make_entry(-9);
    for file_id in 0..=32 {
        make_entry(file_id);
    }
    let max_int32 = i64::from(i32::MAX);
    for file_id in max_int32..=max_int32 + 32 {
        make_entry(file_id);
    }
    let max_int64 = i64::MAX;
    for file_id in max_int64 - 32..max_int64 {
        make_entry(file_id);
    }
    make_entry(max_int64); // have an entry for the maximum int64 value

    // these exist:
    assert!(db.has_file_ids(&[4]));
    assert!(db.has_file_ids(&[-9]));
    assert!(db.has_file_ids(&[8, 25, 31]));
    assert!(db.has_file_ids(&[max_int32 + 4, max_int64 - 17]));
    assert!(db.has_file_ids(&[max_int64 - 17]));
    assert!(db.has_file_ids(&[max_int64]));

    // these don't:
    assert!(!db.has_file_ids(&[-8]));
    assert!(!db.has_file_ids(&[-16]));
    assert!(!db.has_file_ids(&[-(max_int32 + 4)]));
    assert!(!db.has_file_ids(&[max_int64 - 33]));
    assert!(!db.has_file_ids(&[max_int32 + 33]));
    assert!(!db.has_file_ids(&[i64::from(i32::MIN)]));
    assert!(!db.has_file_ids(&[i64::MIN]));

    // as fileids are padded with zeroes, ensure that there is no accidental base-8 conversion done
    // 0o37 (octal) == 31 (decimal) --> 37(dec) does not exist, but 31(dec) does.
    assert!(db.has_file_ids(&[0o37])); // octal
    assert!(!db.has_file_ids(&[37])); // decimal

    assert!(!db.has_file_ids(&[33])); // 33 does not exist...
    assert!(db.has_file_ids(&[33, 25])); // ...but 25 does

    // nothing doesn't exist
    assert!(!db.has_file_ids(&[]));

    // checking for a large amount of file ids should also work just fine, and be reasonably fast.
    // (QBENCHMARK in upstream: run once here.)
    assert!(db.has_file_ids(&all_file_ids));
}

// ---------------------------------------------------------------------------
// Not upstream: compatibility checks against upstream artefacts.

/// Every row of upstream's `test/test_journal.db` (written by a 1.x client)
/// has `phash == c_jhash64(path)` and `pathlen == len(path)`.
#[test]
fn rust_only_phash_matches_upstream_test_journal_db() {
    let conn = Connection::open_with_flags(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/test_journal.db"
        ),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let rows: Vec<(i64, i64, Vec<u8>)> = {
        let mut stmt = conn
            .prepare("SELECT phash, pathlen, path FROM metadata")
            .unwrap();
        collect_rows(&mut stmt, [], |r| {
            (int64_value(r, 0), int64_value(r, 1), ba_value(r, 2))
        })
        .unwrap()
    };
    assert_eq!(rows.len(), 110);
    for (phash, pathlen, path) in rows {
        assert_eq!(
            get_phash(&path),
            phash,
            "{}",
            String::from_utf8_lossy(&path)
        );
        assert_eq!(pathlen as usize, path.len());
    }
}

/// Opening upstream's old-format `test_journal.db` migrates the schema like
/// upstream: all columns added, version row inserted, folder etags invalidated.
#[test]
fn rust_only_open_and_migrate_upstream_test_journal_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_journal.db");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/test_journal.db"
        ),
        &path,
    )
    .unwrap();
    let db = SyncJournalDb::new(&path);
    assert!(db.open());
    {
        let mut inner = db.lock();
        let columns = inner.table_columns("metadata");
        for c in [
            "fileid",
            "remotePerm",
            "filesize",
            "ignoredChildrenRemote",
            "contentChecksum",
            "contentChecksumTypeId",
            "e2eMangledName",
            "isE2eEncrypted",
            "e2eCertificateFingerprint",
            "isShared",
            "lastShareStateFetchedTimestmap",
            "sharedByMe",
            "lock",
            "lockType",
            "lockOwnerDisplayName",
            "lockOwnerId",
            "lockOwnerEditor",
            "lockTime",
            "lockTimeout",
            "lockToken",
            "isLivePhoto",
            "livePhotoFile",
            "quotaBytesUsed",
            "quotaBytesAvailable",
        ] {
            assert!(columns.iter().any(|x| x == c), "missing column {c}");
        }
        let blacklist = inner.table_columns("blacklist");
        for c in [
            "lastTryTime",
            "ignoreDuration",
            "renameTarget",
            "errorCategory",
            "requestId",
        ] {
            assert!(
                blacklist.iter().any(|x| x == c),
                "missing blacklist column {c}"
            );
        }
        let version: (i64, i64, i64) = inner
            .conn()
            .query_row("SELECT major, minor, patch FROM version", [], |r| {
                Ok((int64_value(r, 0), int64_value(r, 1), int64_value(r, 2)))
            })
            .unwrap();
        assert_eq!(
            version,
            (CLIENT_VERSION.0, CLIENT_VERSION.1, CLIENT_VERSION.2)
        );
    }
    let rec = db.get_file_record(b"documents/sync.log").unwrap().unwrap();
    assert_eq!(rec.inode, 1709516);
    assert_eq!(rec.modtime, 1391687536);
    assert_eq!(rec.item_type, ItemType::File);
    assert_eq!(rec.etag, b"52f377d3db044");
    assert!(rec.remote_perm.is_null());
    assert_eq!(rec.folder_quota, FolderQuota::default());

    let mut children = Vec::new();
    db.list_files_in_path(b"documents", |r| children.push(r.path_str()))
        .unwrap();
    assert!(children.contains(&"documents/sync.log".to_owned()));
    assert!(children.iter().all(|p| p.matches('/').count() == 1));
}

/// `parent_hash()` matches `getPHash(parent)`, and the `metadata_parent`
/// index exists so the official client can open journals we wrote.
#[test]
fn rust_only_parent_hash_function_and_index() {
    let f = fixture();
    assert!(f.db.open());
    let inner = f.db.lock();
    let conn = inner.conn();
    for (path, parent) in [("a/b/c.txt", "a/b"), ("top.txt", ""), ("a/b", "a")] {
        let h: i64 = conn
            .query_row("SELECT parent_hash(?1)", [path], |r| r.get(0))
            .unwrap();
        assert_eq!(h, get_phash(parent.as_bytes()), "{path}");
    }
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='metadata_parent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

/// Journal mode is WAL and locking mode EXCLUSIVE, like upstream on Linux.
#[test]
fn rust_only_wal_and_exclusive_locking() {
    let f = fixture();
    assert!(f.db.open());
    let inner = f.db.lock();
    let mode: String = inner
        .conn()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    let locking: String = inner
        .conn()
        .query_row("PRAGMA locking_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locking.to_ascii_lowercase(), "exclusive");
}

/// `makeDbName` = `.sync_` + first 6 bytes of md5("user@url:path") in hex.
#[test]
fn rust_only_make_db_name() {
    let dir = tempfile::tempdir().unwrap();
    let name = make_db_name(dir.path(), "https://cloud.example.com", "/", "alice");
    // md5("alice@https://cloud.example.com:/") computed independently.
    let digest = Md5::digest(b"alice@https://cloud.example.com:/");
    let expected: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(name, format!(".sync_{expected}.db"));
    assert_eq!(name.len(), ".sync_".len() + 12 + ".db".len());
    // The probe file is removed again.
    assert!(!dir.path().join(&name).exists());
}

/// The null/empty remote permission distinction survives a round trip.
#[test]
fn rust_only_remote_perm_null_vs_empty() {
    let f = fixture();
    let mut rec = SyncJournalFileRecord {
        path: b"p".to_vec(),
        modtime: 1,
        ..Default::default()
    };
    f.db.set_file_record(&rec).unwrap();
    assert!(
        f.db.get_file_record(b"p")
            .unwrap()
            .unwrap()
            .remote_perm
            .is_null()
    );
    rec.remote_perm = RemotePermissions::from_db_value(b" ");
    f.db.set_file_record(&rec).unwrap();
    let stored = f.db.get_file_record(b"p").unwrap().unwrap();
    assert!(!stored.remote_perm.is_null());
    assert_eq!(stored.remote_perm.to_db_value(), b" ");
}

#[test]
fn rust_only_find_path_in_selective_sync_list() {
    let list: Vec<String> = ["A/", "B/sub/", "C/"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(find_path_in_selective_sync_list(&list, "A"));
    assert!(find_path_in_selective_sync_list(&list, "A/x"));
    assert!(!find_path_in_selective_sync_list(&list, "AB"));
    assert!(!find_path_in_selective_sync_list(&list, "B"));
    assert!(find_path_in_selective_sync_list(&list, "B/sub/deeper"));
    assert!(find_path_in_selective_sync_list(
        &["/".to_owned()],
        "anything"
    ));

    let f = fixture();
    let l =
        f.db.add_selective_sync_lists(SelectiveSyncListType::BlackList, "Z");
    assert_eq!(l, vec!["Z/".to_owned()]);
    let l =
        f.db.add_selective_sync_lists(SelectiveSyncListType::BlackList, "A/");
    assert_eq!(l, vec!["A/".to_owned(), "Z/".to_owned()]);
    let l =
        f.db.remove_selective_sync_lists(SelectiveSyncListType::BlackList, "Z");
    assert_eq!(l, vec!["A/".to_owned()]);
    assert_eq!(
        f.db.get_selective_sync_list(SelectiveSyncListType::BlackList)
            .unwrap(),
        vec!["A/".to_owned()]
    );
}

#[test]
fn rust_only_autotest_fail_counter() {
    let f = fixture();
    // Like upstream, opening runs checkConnect() several times (tableColumns()
    // calls it), so a small counter makes the open itself fail.
    f.db.set_autotest_fail_counter(1);
    assert!(!f.db.open());
    f.db.set_autotest_fail_counter(-1);
    assert!(f.db.open());
    f.db.set_autotest_fail_counter(1);
    assert!(f.db.delete_file_record("x", false).is_ok()); // counter 1 -> 0
    assert!(f.db.delete_file_record("x", false).is_err()); // counter 0 -> fail
    assert!(f.db.delete_file_record("x", false).is_ok()); // -1: disabled again
}
