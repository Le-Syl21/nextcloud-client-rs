# Test parity with upstream

Reference: nextcloud/desktop tag **v34.0.5**, commit
`62ebad6043b1e7c8e319f41be25d41f5a2c733e5`.

Every upstream test function of the test files covering code ported so far
is listed below. Rust tests keep the upstream name in snake_case. Status:

- **ported**: every upstream assertion is ported and passes;
- **adapted**: ported, but the mechanics had to change (noted);
- **pending**: needs code from a later phase (noted);
- **n/a**: does not apply to the port (reason given).

Tests marked *derived* or *rust_only* are additions without an upstream
counterpart; they are listed at the end of each section.

## Summary

| Upstream file | Functions | Ported/adapted | Pending | n/a |
|---|---:|---:|---:|---:|
| test/testsyncjournaldb.cpp | 14 | 12 | 0 | 2 |
| test/testexcludedfiles.cpp | 23 | 22 | 0 | 1 |
| test/testchecksumvalidator.cpp | 9 | 8 | 0 | 1 |
| test/csync/std_tests/check_std_c_jhash.c | 6 | 6 | 0 | 0 |
| test/testsyncconflict.cpp | 16 | 1 | 14 | 1 |
| test/testutility.cpp | 17 | 1 | 0 | 16 |
| test/testownsql.cpp | 12 | 0 | 0 | 12 |
| **Total** | **97** | **50** | **14** | **33** |

Phase 0 gate: every test function whose subject is a Phase 0 library
(journal, jhash, exclude engine, checksums, remote permissions, conflict-name
helpers) is ported and green. The 14 pending ones are FakeFolder sync tests.

## test/testsyncjournaldb.cpp → `crates/nc-journal/src/journal/tests.rs`

Upstream runs all functions against one shared database; here each test gets
a fresh one (no upstream assertion depends on a previous function's state).

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (Qt logger / QStandardPaths test mode) |
| cleanupTestCase | — | n/a (tempdir cleanup is automatic) |
| testFileRecord | test_file_record | ported |
| testFolderQuota | test_folder_quota | ported |
| testFolderMigration | test_folder_migration | ported (uses the private `table_columns`, `remove_column`, `update_metadata_table_structure`, `has_default_value`) |
| testFileRecordChecksum | test_file_record_checksum | ported |
| testDownloadInfo | test_download_info | ported |
| testUploadInfo | test_upload_info | ported |
| testNumericId | test_numeric_id | ported |
| testConflictRecord | test_conflict_record | ported |
| testAvoidReadFromDbOnNextSync | test_avoid_read_from_db_on_next_sync | ported |
| testRecursiveDelete | test_recursive_delete | ported |
| testPinState | test_pin_state | ported |
| testHasFileIds | test_has_file_ids | ported (`QBENCHMARK` body run once) |

rust_only: `rust_only_phash_matches_upstream_test_journal_db` (phash and
pathlen of all 110 rows of upstream `test/test_journal.db`),
`rust_only_open_and_migrate_upstream_test_journal_db` (schema migration of
that 1.x journal, version row, `list_files_in_path` via `parent_hash`),
`rust_only_parent_hash_function_and_index`, `rust_only_wal_and_exclusive_locking`,
`rust_only_make_db_name`, `rust_only_remote_perm_null_vs_empty`,
`rust_only_find_path_in_selective_sync_list`, `rust_only_autotest_fail_counter`.

## test/testexcludedfiles.cpp → `crates/nc-journal/src/exclude/tests.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (Qt logger / test mode) |
| testFun | test_fun | ported |
| check_csync_exclude_add | check_csync_exclude_add | ported |
| check_csync_exclude_add_per_dir | check_csync_exclude_add_per_dir | ported |
| check_csync_excluded | check_csync_excluded | ported (`_WIN32` block under `cfg(windows)`) |
| check_csync_excluded_per_dir | check_csync_excluded_per_dir | ported (Qt temp location → tempdir) |
| check_csync_excluded_traversal_per_dir | check_csync_excluded_traversal_per_dir | ported |
| check_csync_excluded_traversal | check_csync_excluded_traversal | ported |
| check_csync_dir_only | check_csync_dir_only | ported |
| check_csync_pathes | check_csync_pathes | ported |
| check_csync_wildcards | check_csync_wildcards | ported |
| check_csync_regex_translation | check_csync_regex_translation | ported (same generated PCRE text as upstream) |
| check_csync_bname_trigger | check_csync_bname_trigger | ported |
| check_csync_is_windows_reserved_word | check_csync_is_windows_reserved_word | ported |
| check_csync_excluded_performance1 | check_csync_excluded_performance1 | adapted (benchmark loop run once, N=1000) |
| check_csync_excluded_performance2 | check_csync_excluded_performance2 | adapted (sets up its own state instead of reusing performance1's) |
| check_csync_exclude_expand_escapes | check_csync_exclude_expand_escapes | ported |
| check_version_directive | check_version_directive | ported |
| testAddExcludeFilePath_addSameFilePath_listSizeDoesNotIncrease | test_add_exclude_file_path_add_same_file_path_list_size_does_not_increase | ported |
| testAddExcludeFilePath_addDifferentFilePaths_listSizeIncrease | test_add_exclude_file_path_add_different_file_paths_list_size_increase | ported |
| testAddExcludeFilePath_addDefaultExcludeFile_returnCorrectMap | test_add_exclude_file_path_add_default_exclude_file_return_correct_map | ported |
| testReloadExcludeFiles_fileDoesNotExist_returnTrue | test_reload_exclude_files_file_does_not_exist_return_true | ported |
| testReloadExcludeFiles_fileExists_returnTrue | test_reload_exclude_files_file_exists_return_true | ported |

rust_only: `rust_only_pcre_translation_keeps_pcre_semantics`,
`rust_only_default_list_patterns_all_compile`,
`rust_only_version_directive_skips_next_line`.

The suite also passes with `OWNCLOUD_TEST_CASE_PRESERVING=1` (Windows/macOS
case-insensitive mode).

## test/testchecksumvalidator.cpp → `crates/nc-journal/src/checksums/tests.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | fixture function | ported |
| testMd5Calc | test_md5_calc | ported (compared against `md5sum`) |
| testSha1Calc | test_sha1_calc | ported (compared against `sha1sum`) |
| testUploadChecksummingAdler | test_upload_checksumming_adler | adapted (synchronous `ComputeChecksum::start`) |
| testUploadChecksummingMd5 | test_upload_checksumming_md5 | adapted (idem) |
| testUploadChecksummingSha1 | test_upload_checksumming_sha1 | adapted (idem) |
| testDownloadChecksummingAdler | test_download_checksumming_adler | adapted (exact mismatch / unknown-type messages and reasons checked on the `Result`) |
| testDeletingComputeChecksumBeforeCalculationCompletionDoesNotCrash | test_deleting_compute_checksum_before_calculation_completion_does_not_crash | adapted (64 cancellations through a cancel flag on `/dev/zero`; upstream deletes the Qt object 4096 times) |
| cleanupTestCase | — | n/a (empty) |

`slotUpValidated`, `slotDownValidated` and `slotDownError` are Qt slots used
by the tests, not test functions; they became assertions on the `Result`.

derived: `derived_find_best_checksum`, `derived_checksum_header_helpers`,
`derived_calculator_encodings`, `derived_validate_malformed_and_empty`,
`derived_csync_checksum_hook`.

## Remote permissions (no upstream test file) → `crates/nc-journal/src/remote_permissions.rs`

Upstream has no dedicated test; its behaviour is exercised by
test/testpermissions.cpp through FakeFolder (Phase 1). derived:
`null_and_empty_db_values`, `letters_round_trip_in_canonical_order`,
`testpermissions_strings` (all `fromServerString` strings used in
testpermissions.cpp), `unknown_letters_ignored_and_set_unset`,
`share_attributes_download_forbidden`, `mount_root_algorithm`,
`utf16_truncation_quirks`.

## test/csync/std_tests/check_std_c_jhash.c → `crates/nc-journal/tests/jhash.rs`

| Upstream | Rust | Status |
|---|---|---|
| check_c_jhash_trials | check_c_jhash_trials | ported (upstream's final assertion is dead code; kept as is) |
| check_c_jhash_alignment_problems | check_c_jhash_alignment_problems | ported |
| check_c_jhash_null_strings | check_c_jhash_null_strings | ported |
| check_c_jhash64_trials | check_c_jhash64_trials | ported (idem) |
| check_c_jhash64_alignment_problems | check_c_jhash64_alignment_problems | ported |
| check_c_jhash64_null_strings | check_c_jhash64_null_strings | ported |

rust_only: `upstream_c_vectors_bit_exact`: 418 vectors (lengths 0..100 with
four initial values each, plus 14 real-world paths incl. NFC/NFD and non-Latin
names) printed by a C program compiled against upstream `c_jhash.h`
(`tools/jhash-oracle/run.sh <upstream checkout>` regenerates them).

## test/testsyncconflict.cpp

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a |
| testConflictFileBaseName_data + testConflictFileBaseName | `crates/nc-journal/tests/utility.rs::test_conflict_file_base_name` | ported (all 19 data rows) |
| testNoUpload | — | pending (FakeFolder + nc-sync, Phase 1) |
| testUploadAfterDownload | — | pending (Phase 1) |
| testSeparateUpload | — | pending (Phase 1) |
| testDownloadingConflictFile | — | pending (Phase 1) |
| testConflictRecordRemoval1 | — | pending (Phase 1) |
| testConflictRecordRemoval2 | — | pending (Phase 1) |
| testLocalDirRemoteFileConflict | — | pending (Phase 1) |
| testLocalFileRemoteDirConflict | — | pending (Phase 1) |
| testTypeConflictWithMove | — | pending (Phase 1) |
| testTypeChange | — | pending (Phase 1) |
| testEtagChangeFileNotChangedGeneratesNoConflicts | — | pending (Phase 1) |
| testEtagChangeFileChangedGeneratesConflicts | — | pending (Phase 1) |
| testRemoveRemove | — | pending (Phase 1) |

(`testConflictFileBaseName_data` is counted with its test function.)

## test/testutility.cpp

Only the parts of `utility.cpp` needed by the journal and the exclude engine
are ported so far.

| Upstream | Rust | Status |
|---|---|---|
| testFsCasePreserving | `crates/nc-journal/tests/utility.rs::test_fs_case_preserving` | ported |
| initTestCase | — | n/a |
| testFormatFingerprint, testOctetsToString, testLaunchOnStartup, testDurationToDescriptiveString, testTimeAgo, testSetupFavLink | — | n/a (GUI / desktop integration helpers, not ported) |
| testFileNamesEqual, testSanitizeForFileName_data, testSanitizeForFileName, testNormalizeEtag, testIsPathWindowsDrivePartitionRoot, testFullRemotePathToRemoteSyncRootRelative, testExpandCommandLineOptionValues_data, testExpandCommandLineOptionValues | — | n/a for now: the functions are not ported yet; they become **pending** when the engine needs them (`normalizeEtag` and `fullRemotePathToRemoteSyncRootRelative` in Phase 1) |

## test/testownsql.cpp

All 12 functions (initTestCase, testOpenDb, testCreate, testIsSelect,
testInsert, testInsert2, testSelect, testSelect2, testPragma, testUnicode,
testReadUnicode, testDestructor) test the `SqlDatabase`/`SqlQuery` wrapper
around the SQLite C API, which is replaced by `rusqlite` rather than ported:
**n/a**. The wrapper semantics that matter for the on-disk format (QByteArray
bound as TEXT, null QString bound as NULL, `sqlite3_column_*` conversions,
`exec()` not stepping PRAGMA/SELECT statements, quick_check + recreate on a
broken database) are reproduced in `journal/mod.rs` and covered by the journal
tests.

## test/syncenginetestutils.{h,cpp} (harness, no test functions)

Ported design and status per class: see the crate documentation of
`crates/nc-testutils/src/lib.rs`. Its own unit tests (`file_info::tests`,
`server::tests`, `disk::tests`, `folder::tests`, `path::tests`) are derived.
