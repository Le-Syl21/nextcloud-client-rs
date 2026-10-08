# Test parity with upstream

Reference: nextcloud/desktop tag **v34.0.5**, commit
`62ebad6043b1e7c8e319f41be25d41f5a2c733e5`.

Every upstream test function of the test files covering code ported so far
is listed below. Rust tests keep the upstream name in snake_case. Status:

- **ported**: every upstream assertion is ported and passes;
- **adapted**: ported, but the mechanics had to change (noted);
- **pending**: not passing yet or needs code not ported yet (noted; the Rust
  test exists and is `#[ignore]`d);
- **n/a**: does not apply to the port (reason given).

Functions count every test slot, `initTestCase`/`init`/`cleanup` included;
a `_data` function is counted with its test function, whose data rows all
run inside the one Rust test. Tests marked *derived* or *rust_only* are
additions without an upstream counterpart.

Not counted here: test files of later phases not ported yet, GUI tests, and the
virtual files / end-to-end encryption test files (out of scope).

## Summary

| Upstream file | Functions | Ported/adapted | Pending | n/a |
|---|---:|---:|---:|---:|
| test/testsyncjournaldb.cpp | 14 | 12 | 0 | 2 |
| test/testexcludedfiles.cpp | 23 | 22 | 0 | 1 |
| test/testchecksumvalidator.cpp | 9 | 8 | 0 | 1 |
| test/csync/std_tests/check_std_c_jhash.c | 6 | 6 | 0 | 0 |
| test/testownsql.cpp | 12 | 0 | 0 | 12 |
| test/testutility.cpp | 14 | 7 | 0 | 7 |
| test/testnetrcparser.cpp | 6 | 4 | 0 | 2 |
| test/testxmlparse.cpp | 13 | 10 | 0 | 3 |
| test/testconcaturl.cpp | 2 | 1 | 0 | 1 |
| test/testcapabilities.cpp | 27 | 26 | 0 | 1 |
| test/testsyncfileitem.cpp | 3 | 1 | 0 | 2 |
| test/testnextcloudpropagator.cpp | 7 | 6 | 0 | 1 |
| test/testsyncengine.cpp | 57 | 55 | 0 | 2 |
| test/testsyncmove.cpp | 30 | 29 | 0 | 1 |
| test/testsyncconflict.cpp | 15 | 14 | 0 | 1 |
| test/testremotediscovery.cpp | 7 | 6 | 0 | 1 |
| test/testsyncdelete.cpp | 3 | 2 | 0 | 1 |
| test/testselectivesync.cpp | 3 | 2 | 0 | 1 |
| test/testallfilesdeleted.cpp | 8 | 7 | 0 | 1 |
| test/testlocaldiscovery.cpp | 24 | 23 | 0 | 1 |
| test/testpermissions.cpp | 18 | 17 | 0 | 1 |
| test/testdatabaseerror.cpp | 2 | 1 | 0 | 1 |
| test/testlockedfiles.cpp | 8 | 1 | 0 | 7 |
| test/testlongpath.cpp | 3 | 1 | 0 | 2 |
| test/testchunkingng.cpp | 18 | 17 | 0 | 1 |
| test/testuploadreset.cpp | 2 | 1 | 0 | 1 |
| test/testdownload.cpp | 6 | 5 | 0 | 1 |
| test/testblacklist.cpp | 2 | 1 | 0 | 1 |
| test/testasyncop.cpp | 2 | 1 | 0 | 1 |
| test/testinotifywatcher.cpp | 4 | 3 | 0 | 1 |
| test/testfolderwatcher.cpp | 15 | 14 | 0 | 1 |
| test/testpushnotifications.cpp | 15 | 14 | 0 | 1 |
| test/testfolderman.cpp | 10 | 4 | 0 | 6 |
| test/testforcesyncnow.cpp | 3 | 2 | 0 | 1 |
| test/testaccountmanager.cpp | 7 | 1 | 0 | 6 |
| test/testaccount.cpp | 5 | 2 | 0 | 3 |
| test/testsyncfilestatustracker.cpp | 14 | 13 | 0 | 1 |
| test/testlockfile.cpp | 28 | 27 | 0 | 1 |
| test/testfilesystem.cpp | 7 | 4 | 0 | 3 |
| test/testnextcloudcmdprovisioning.cpp | 9 | 9 | 0 | 0 |
| test/testremotewipe.cpp | 2 | 1 | 0 | 1 |
| test/testclientstatusreporting.cpp | 4 | 4 | 0 | 0 |
| test/testcookies.cpp | 1 | 0 | 0 | 1 |
| test/testconnectionvalidator.cpp | 2 | 0 | 0 | 2 |
| test/testfolder.cpp | 2 | 0 | 0 | 2 |
| **Total** | **472** | **384** | **0** | **88** |

Phase 1 gate: every FakeFolder test file in the Phase 1 list
(testsyncengine, testsyncmove, testsyncconflict, testchunkingng,
testlocaldiscovery, testremotediscovery, testpermissions,
testallfilesdeleted, testblacklist, testdownload, testuploadreset,
testselectivesync, testdatabaseerror, testlockedfiles, testlongpath,
testsyncdelete) is ported, none pending. The two
`HAVE_QHTTPSERVER` tests of testnextcloudpropagator are ported in Phase 2
(decompression safety check, direct download URLs).

# Phase 0 libraries

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

## test/testutility.cpp

The functions whose subject is ported are ported where that code lives.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a |
| testFsCasePreserving | `crates/nc-journal/tests/utility.rs::test_fs_case_preserving` | ported |
| testOctetsToString | `crates/nc-sync/src/utility.rs::tests::test_octets_to_string` | ported (C locale) |
| testDurationToDescriptiveString | `nc-sync … test_duration_to_descriptive_string` | ported (untranslated plurals, as upstream without a translation loaded) |
| testSanitizeForFileName_data + testSanitizeForFileName | `nc-sync … test_sanitize_for_file_name` | ported (3 rows) |
| testNormalizeEtag | `nc-sync … test_normalize_etag` | ported |
| testFullRemotePathToRemoteSyncRootRelative | `nc-sync … test_full_remote_path_to_remote_sync_root_relative` | ported |
| testFormatFingerprint, testLaunchOnStartup, testTimeAgo, testSetupFavLink | — | n/a (GUI / desktop integration helpers) |
| testFileNamesEqual | — | n/a (`fileNamesEqual` is only used by the GUI folder watcher) |
| testIsPathWindowsDrivePartitionRoot | — | n/a (Windows only; the function is not needed on Linux) |
| testExpandCommandLineOptionValues_data + testExpandCommandLineOptionValues | `crates/ncsync/src/main.rs::tests::test_expand_command_line_option_values` | ported (13 rows; `cmd.cpp` applies it before parsing, so does `ncsync`) |

## test/testnetrcparser.cpp → `crates/ncsync/src/netrc.rs` (tests module)

| Upstream | Rust | Status |
|---|---|---|
| initTestCase, cleanupTestCase | fixture helper | n/a (files are written to a tempdir per test) |
| testValidNetrc | test_valid_netrc | ported (the `QEXPECT_FAIL` on quoted names asserts the known limitation) |
| testEmptyNetrc | test_empty_netrc | ported |
| testValidNetrcWithDefault | test_valid_netrc_with_default | ported |
| testInvalidNetrc | test_invalid_netrc | ported |

# Network layer and unit tests

## test/testxmlparse.cpp → `crates/nc-dav/tests/testxmlparse.rs`

Signal slots map onto the parse result: entries count as emitted even on
failure (`LsColError::partial`); subfolders and success only on `Ok`.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase, init, cleanup | — | n/a (Qt fixtures) |
| testParser1 | test_parser1 | ported |
| testParserBrokenXml | test_parser_broken_xml | ported |
| testParserEmptyXmlNoDav | test_parser_empty_xml_no_dav | ported |
| testParserEmptyXml | test_parser_empty_xml | ported |
| testParserTruncatedXml | test_parser_truncated_xml | ported |
| testParserBogfusHref1 | test_parser_bogfus_href1 | ported |
| testParserBogfusHref2 | test_parser_bogfus_href2 | ported |
| testParserDenormalizedPath | test_parser_denormalized_path | ported |
| testParserDenormalizedPathOutsideNamespace | test_parser_denormalized_path_outside_namespace | ported |
| testHrefUrlEncoding | test_href_url_encoding | ported |

## test/testconcaturl.cpp → `crates/nc-dav/tests/testconcaturl.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a |
| testFolder (+_data, 15 rows) | test_folder | ported |

## test/testcapabilities.cpp → `crates/nc-dav/tests/testcapabilities.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a |
| testPushNotificationsAvailable_* (7 functions) | test_push_notifications_available_* | ported |
| testPushNotificationsWebSocketUrl_urlAvailable_returnUrl | same in snake_case | ported |
| testUserStatus_* (3) | test_user_status_* | ported |
| testUserStatusSupportsEmoji_* (3) | test_user_status_supports_emoji_* | ported |
| testUserStatusSupportsBusy_* (4) | test_user_status_supports_busy_* | ported |
| testShareDefaultPermissions_* (2) | test_share_default_permissions_* | ported |
| testBulkUploadAvailable_bulkUploadAvailable_returnTrue | same in snake_case | ported |
| testFilesLockAvailable_filesLockAvailable_returnTrue | same | ported |
| testSupport_hasValidSubscription_returnTrue | same | ported |
| testSupport_desktopEnterpriseChannel_returnString | same | adapted (`Option` accessor; the `ConfigFile::defaultUpdateChannel()` fallback is GUI updater config, not ported) |
| testServerHasClientIntegration_returnTrue | same | ported |
| testFileActionsByMimeType_returnContextMenu | same | adapted (`QMimeDatabase` lookups replaced by the shared-mime-info types they return, with Qt's implicit `text/plain` / `application/octet-stream` parents) |

## test/testsyncfileitem.cpp → `crates/nc-sync/tests/testsyncfileitem.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase, cleanupTestCase | — | n/a |
| testComparator (+_data, 6 rows) | test_comparator | ported |

## test/testnextcloudpropagator.cpp → `crates/nc-testutils/tests/testnextcloudpropagator.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a |
| testUpdateErrorFromSession | test_update_error_from_session | ported (upstream body is `QVERIFY(true)`) |
| testTmpDownloadFileNameGeneration | test_tmp_download_file_name_generation | ported |
| testParseEtag | test_parse_etag | ported |
| testParseException | test_parse_exception | ported |
| testGETFileJobDecompressionThreshold (`HAVE_QHTTPSERVER`) | test_get_file_job_decompression_threshold | adapted (a minimal local HTTP/1.1 server, `tests/common/http_server.rs`, replaces `QHttpServer`; the real reqwest transport with the ported Qt decompression check; `QSignalSpy` on `finishedSignal` becomes `GetFileResult::finished_signal`; `spy.wait(1000)` is a plain await) |
| testDirectUrlCredentials (`HAVE_QHTTPSERVER`) | test_direct_url_credentials | adapted (same local server; the `QVERIFY`s of the route handlers become assertions on the recorded requests after the four downloads: no `Authorization` on the 4 requests of the direct download server, `Authorization` on the 2 of the Nextcloud server) |

## test/testasyncop.cpp → `crates/nc-testutils/tests/testasyncop.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a |
| asyncUploadOperations | async_upload_operations | adapted (the abort inside the override goes through `AbortTrigger` + `sync_once_abortable`; about 57 s because the 5 s PollJob retry timer is kept) |

# FakeFolder sync tests

## test/testsyncengine.cpp → `crates/nc-testutils/tests/testsyncengine.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase, init | — | n/a (logger setup, test mode, UTF-8 codec: always UTF-8 in the port) |
| testFileDownload | test_file_download | ported |
| testFileUpload | test_file_upload | ported |
| testDirDownload | test_dir_download | ported |
| testDirUpload | test_dir_upload | ported |
| testDirUploadWithDelayedAlgorithm | test_dir_upload_with_delayed_algorithm | adapted (`QSKIP("bulk upload is disabled")` upstream; runs with the experimental opt-in `SyncOptions::bulk_upload`) |
| testDirUploadWithDelayedAlgorithmWithNewChecksum | test_dir_upload_with_delayed_algorithm_with_new_checksum | adapted (`QSKIP("bulk upload is disabled")` upstream; runs with the experimental opt-in `SyncOptions::bulk_upload`) |
| testLocalDelete | test_local_delete | ported |
| testRemoteDelete | test_remote_delete | ported |
| testEmlLocalChecksum | test_eml_local_checksum | ported |
| testSelectiveSyncBug | test_selective_sync_bug | ported |
| testDirEtagAfterIncompleteSync | test_dir_etag_after_incomplete_sync | ported |
| testDirDownloadWithError | test_dir_download_with_error | ported |
| testFakeConflict | test_fake_conflict | ported |
| testSyncFileItemProperties | test_sync_file_item_properties | ported |
| testInsufficientRemoteStorage | test_insufficient_remote_storage | ported |
| testChecksumValidation | test_checksum_validation | ported |
| testInvalidFilenameRegex | test_invalid_filename_regex | ported |
| testDiscoveryHiddenFile | test_discovery_hidden_file | ported |
| testNoLocalEncoding | test_no_local_encoding | adapted (UTF-8 part only: the port always uses UTF-8 file names, no `QTextCodec::setCodecForLocale`) |
| testUploadV1Multiabort | test_upload_v1_multiabort | ported |
| testPropagatePermissions | test_propagate_permissions | ported |
| testEmptyLocalButHasRemote | test_empty_local_but_has_remote | ported |
| testDirectoryInitialMtime | test_directory_initial_mtime | ported |
| testLocalFileInitialMtime | test_local_file_initial_mtime | ported |
| testErrorsWithBulkUpload | test_errors_with_bulk_upload | adapted (`QSKIP("bulk upload is disabled")` upstream; runs with the experimental opt-in `SyncOptions::bulk_upload`, also set in the fresh `SyncOptions`, with `minimum_file_age_for_upload` 0 since that is a `SyncEngine` static upstream) |
| testNetworkErrorsWithBulkUpload | test_network_errors_with_bulk_upload | adapted (`QSKIP("bulk upload is disabled")` upstream; runs with the experimental opt-in `SyncOptions::bulk_upload`, also set in the fresh `SyncOptions`, with `minimum_file_age_for_upload` 0 since that is a `SyncEngine` static upstream) |
| testNetworkErrorsWithSmallerBatchSizes | test_network_errors_with_smaller_batch_sizes | adapted (`QSKIP("bulk upload is disabled")` upstream; runs with the experimental opt-in `SyncOptions::bulk_upload`; upstream's reply lambda looks up `X-File-Path` in lower-cased headers, so no part fails, kept as is) |
| testRemoteMoveFailedInsufficientStorageLocalMoveRolledBack | test_remote_move_failed_insufficient_storage_local_move_rolled_back | ported |
| testRemoteMoveFailedForbiddenLocalMoveRolledBack | test_remote_move_failed_forbidden_local_move_rolled_back | ported |
| testFolderWithFilesInError | test_folder_with_files_in_error | ported |
| testInvalidMtimeRecoveryAtStart | test_invalid_mtime_recovery_at_start | ported |
| testInvalidMtimeRecovery | test_invalid_mtime_recovery | ported |
| testLocalInvalidMtimeCorrection | test_local_invalid_mtime_correction | ported |
| testLocalInvalidMtimeCorrectionBulkUpload | test_local_invalid_mtime_correction_bulk_upload | adapted (`QSKIP("bulk upload is disabled")` upstream; runs with the experimental opt-in `SyncOptions::bulk_upload`) |
| testServerUpdatingMTimeShouldNotCreateConflicts | test_server_updating_mtime_should_not_create_conflicts | ported |
| testFolderRemovalWithCaseClash | test_folder_removal_with_case_clash | ported (both _data rows) |
| testServer_caseClash_createConflict | test_server_case_clash_create_conflict | ported |
| testServer_subFolderCaseClash_createConflict | test_server_sub_folder_case_clash_create_conflict | ported |
| testServer_caseClash_createConflictOnMove | test_server_case_clash_create_conflict_on_move | ported |
| testServer_subFolderCaseClash_createConflictOnMove | test_server_sub_folder_case_clash_create_conflict_on_move | ported |
| testServer_caseClash_createConflictAndSolveIt | test_server_case_clash_create_conflict_and_solve_it | ported (Linux branch: no case clash; the `CaseClashConflictSolver` branch does not run on Linux upstream either) |
| testServer_subFolderCaseClash_createConflictAndSolveIt | test_server_sub_folder_case_clash_create_conflict_and_solve_it | ported (idem) |
| testServer_caseClash_createConflict_thenRemoveOneRemoteFile | test_server_case_clash_create_conflict_then_remove_one_remote_file | ported |
| testServer_caseClash_createDiverseConflictsInsideOneFolderAndSolveThem | test_server_case_clash_create_diverse_conflicts_inside_one_folder_and_solve_them | ported (Linux branch only builds the tree, like upstream) |
| testExistingFolderBecameBig | test_existing_folder_became_big | adapted (`ConfigFile::setNotifyExistingFoldersOverLimit` → `SyncOptions::notify_existing_folders_over_limit`) |
| testFileDownloadWithUnicodeCharacterInName | test_file_download_with_unicode_character_in_name | ported |
| testRemoteTypeChangeExistingLocalMustGetRemoved | test_remote_type_change_existing_local_must_get_removed | ported |
| testRemoveAllFilesWithNextcloudCmd | test_remove_all_files_with_nextcloud_cmd | adapted (`ConfigFile().setPromptDeleteFiles(true)` → `prompt_delete_files`) |
| testRemoveAllFilesWithoutNextcloudCmd | test_remove_all_files_without_nextcloud_cmd | adapted (idem) |
| testSyncReadOnlyLnkWindowsShortcuts | test_sync_read_only_lnk_windows_shortcuts | ported |
| testSyncLongPaths | test_sync_long_paths | ported |
| testCreateFileWithTrailingLeadingSpaces_local_automatedRenameBeforeUpload | test_create_file_with_trailing_leading_spaces_local_automated_rename_before_upload | ported |
| testTouchedFilesWhenChangingFolderPermissionsDuringSync | test_touched_files_when_changing_folder_permissions_during_sync | adapted (`touchedFile` observed through the `propagator_event` callback) |
| testSyncFolderNewDeleteConflictExpectDeletion | test_sync_folder_new_delete_conflict_expect_deletion | ported |
| testUploadWhileFileIsChanging | test_upload_while_file_is_changing | adapted (`execUntilBeforePropagation` → `about_to_propagate` callback that grows the file) |

Bulk upload (`BulkPropagatorJob`, `PutMultiFileJob`) is ported but off by
default: upstream v34.0.5's `isDelayedUploadItem()` returns `false`. The six
bulk upload tests run with the opt-in; `FakePutMultiFileReply` and
`forEachReplyPart` are in the harness (`server::put_multi_file_reply`,
`server::for_each_reply_part`).

## test/testsyncmove.cpp → `crates/nc-testutils/tests/testsyncmove.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger / QStandardPaths setup) |
| testMoveCustomRemoteRoot | test_move_custom_remote_root | ported |
| testRemoteChangeInMovedFolder | test_remote_change_in_moved_folder | ported |
| testSelectiveSyncMovedFolder | test_selective_sync_moved_folder | ported |
| testLocalMoveDetection | test_local_move_detection | ported |
| testLocalExternalStorageRenameDetection | test_local_external_storage_rename_detection | ported |
| testDuplicateFileId (+_data: first ordering, second ordering) | test_duplicate_file_id | ported |
| testMovePropagation | test_move_propagation | ported (settled by running the upstream test binary: upstream's `QCOMPARE(printDbData(...), printDbData(...))` goes through `QTest::toString`, which truncates to 245 characters, so only the start of the trees is compared; the journal of upstream has the same local mtime for the re-uploaded folder `CML` as ours. `print_db_data` reproduces the truncation, see `nc-testutils/src/file_info.rs`) |
| testRenameCaseOnly | test_rename_case_only | ported |
| testMoveAndTypeChange | test_move_and_type_change | ported (the `QCOMPARE` is commented out upstream too, "BUG") |
| testInvertFolderHierarchy | test_invert_folder_hierarchy | ported |
| testDeepHierarchy (+_data: remote, local) | test_deep_hierarchy | ported |
| renameOnBothSides | rename_on_both_sides | ported (a private slot, so QtTest runs it) |
| moveFileToDifferentFolderOnBothSides | move_file_to_different_folder_on_both_sides | ported (idem) |
| testRenameParallelism | test_rename_parallelism | ported |
| testRenameParallelismWithBlacklist | test_rename_parallelism_with_blacklist | ported |
| testMovedWithError (+_data) | test_moved_with_error | adapted (row `Vfs::Off` only; the `WithSuffix` and `WindowsCfApi` rows need VFS: n/a) |
| testRenameDeepHierarchy | test_rename_deep_hierarchy | ported |
| testRenameSameFileInMultiplePaths | test_rename_same_file_in_multiple_paths | ported |
| testBlockRenameTopFolderFromGroupFolder | test_block_rename_top_folder_from_group_folder | ported |
| testAllowRenameChildFolderFromGroupFolder | test_allow_rename_child_folder_from_group_folder | ported |
| testMultipleRenameFromServer | test_multiple_rename_from_server | adapted (`aboutToPropagate` → `about_to_propagate` callback with the assertions inside) |
| testMultipleRenameFromLocal | test_multiple_rename_from_local | ported |
| testRenameComplexScenarioNoRecordLeak | test_rename_complex_scenario_no_record_leak | adapted (`itemCompleted` lambda → `item_completed` callback with a shared journal handle) |
| testRenameFileThatExistsInMultiplePaths | test_rename_file_that_exists_in_multiple_paths | ported |
| testLockTokenClearedOnServerInitiatedRename | test_lock_token_cleared_on_server_initiated_rename | ported |
| testLockedStateClearedOnClientInitiatedRename | test_locked_state_cleared_on_client_initiated_rename | ported |
| testUpload412SchedulesRediscovery | test_upload412_schedules_rediscovery | ported |
| testUpload423ClearsStaleLockState | test_upload423_clears_stale_lock_state | ported |
| testDeletedDirPrefixDoesNotMatchSibling | test_deleted_dir_prefix_does_not_match_sibling | ported |

## test/testsyncconflict.cpp → `crates/nc-testutils/tests/testsyncconflict.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger setup) |
| testNoUpload | test_no_upload | ported |
| testUploadAfterDownload | test_upload_after_download | ported |
| testSeparateUpload | test_separate_upload | ported |
| testDownloadingConflictFile | test_downloading_conflict_file | ported |
| testConflictRecordRemoval1 | test_conflict_record_removal1 | ported |
| testConflictRecordRemoval2 | test_conflict_record_removal2 | ported |
| testConflictFileBaseName_data + testConflictFileBaseName | `crates/nc-journal/tests/utility.rs::test_conflict_file_base_name` | ported (all 19 data rows) |
| testLocalDirRemoteFileConflict | test_local_dir_remote_file_conflict | ported |
| testLocalFileRemoteDirConflict | test_local_file_remote_dir_conflict | ported |
| testTypeConflictWithMove | test_type_conflict_with_move | ported |
| testTypeChange | test_type_change | ported |
| testEtagChangeFileNotChangedGeneratesNoConflicts | test_etag_change_file_not_changed_generates_no_conflicts | ported (upstream `insert(path, 'W')` passes 'W' as the size: 87 bytes, kept) |
| testEtagChangeFileChangedGeneratesConflicts | test_etag_change_file_changed_generates_conflicts | ported (idem) |
| testRemoveRemove | test_remove_remove | ported |

## test/testremotediscovery.cpp → `crates/nc-testutils/tests/testremotediscovery.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger setup; timeouts are always enabled in the port) |
| testRemoteDiscoveryError_data + testRemoteDiscoveryError | test_remote_discovery_error | ported (27 rows; the Timeout row sets the process-wide HTTP timeout through `nc_dav::jobs::set_http_timeout`, like the static `httpTimeout`) |
| testMissingData | test_missing_data | ported |
| testQuotaReportedAsDouble | test_quota_reported_as_double | ported |
| testRootFileIdReceived | test_root_file_id_received | ported |
| testLsColJobDoesNotCrashWhenReplyIsDeletedDuringProcessEvents | test_ls_col_job_does_not_crash_when_reply_is_deleted_during_process_events | adapted (no Qt `deleteLater`: the pending-deletion reply is a plain PROPFIND reply; all assertions kept) |
| testLsColJobSucceedsNormally | test_ls_col_job_succeeds_normally | ported |

## test/testsyncdelete.cpp → `crates/nc-testutils/tests/testsyncdelete.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger setup) |
| testDeleteDirectoryWithNewFile | test_delete_directory_with_new_file | ported |
| issue1329 | issue1329 | ported |

## test/testselectivesync.cpp → `crates/nc-testutils/tests/testselectivesync.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger setup) |
| testSelectiveSyncBigFolders | test_selective_sync_big_folders | ported (`minimumFileAgeForUpload` is a static upstream and a `SyncOptions` field here: reset to 0 after replacing the options) |
| testRestoreSubFolderForDataFingerPrint | test_restore_sub_folder_for_data_finger_print | ported |

## test/testallfilesdeleted.cpp → `crates/nc-testutils/tests/testallfilesdeleted.rs`

Upstream sets `ConfigFile().setPromptDeleteFiles(true)` once in
testAllFilesDeletedKeep and it persists in the test config for the following
functions (testResetServer depends on it); each Rust test sets
`prompt_delete_files = true` itself.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger setup) |
| testAllFilesDeletedKeep (+_data: local, remote) | test_all_files_deleted_keep | ported |
| testAllFilesDeletedDelete (+_data: local, remote) | test_all_files_deleted_delete | ported |
| testNotDeleteMetaDataChange | test_not_delete_meta_data_change | ported |
| testResetServer | test_reset_server | ported |
| testDataFingetPrint (+_data: initial finger print, no initial finger print) | test_data_finget_print | ported |
| testSingleFileRenamed | test_single_file_renamed | ported |
| testSelectiveSyncNoPopup | test_selective_sync_no_popup | ported |

## test/testlocaldiscovery.cpp → `crates/nc-testutils/tests/testlocaldiscovery.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger / QStandardPaths setup) |
| testFileOpenedAsDirectoryCompletesDiscoveryJob | test_file_opened_as_directory_completes_discovery_job | adapted (the job runs synchronously through `discovery_single_local_directory_job`, no thread pool and no 5 s `QTRY` wait) |
| testSelectiveSyncQuotaExceededDataLoss | test_selective_sync_quota_exceeded_data_loss | ported |
| testLocalDiscoveryStyle | test_local_discovery_style | ported |
| testLocalDiscoveryDecision | test_local_discovery_decision | adapted (the `QEXPECT_FAIL` on `!shouldDiscoverLocally("A/X o")` asserts the known false positive, so a fix would fail like a Qt XPASS) |
| testTrackerItemCompletion | test_tracker_item_completion | ported (`LocalDiscoveryTracker` driven from the engine callbacks) |
| testDirectoryAndSubDirectory | test_directory_and_sub_directory | ported |
| testServerBlacklist | test_server_blacklist | ported |
| testServerForbiddenFilenames | test_server_forbidden_filenames | ported |
| testRedownloadDeletedLivePhotoMov | test_redownload_deleted_live_photo_mov | ported |
| testCreateFileWithTrailingSpaces_localAndRemoteTrimmedDoNotExist_renameAndUploadFile | test_create_file_with_trailing_spaces_local_and_remote_trimmed_do_not_exist_rename_and_upload_file | ported (non-Windows branch) |
| testCreateFileWithTrailingSpaces_remoteDontGetRenamedAutomatically | test_create_file_with_trailing_spaces_remote_dont_get_renamed_automatically | ported (non-Windows branch) |
| testCreateLocalPathsWithLeadingAndTrailingSpaces_syncOnSupportingOs | test_create_local_paths_with_leading_and_trailing_spaces_sync_on_supporting_os | ported |
| testCreateFileWithTrailingSpaces_remoteGetRenamedManually | test_create_file_with_trailing_spaces_remote_get_renamed_manually | ported (non-Windows branch) |
| testCreateFileWithTrailingSpaces_localTrimmedAlsoCreated_dontRenameAutomaticallyAndDontUploadFile | test_create_file_with_trailing_spaces_local_trimmed_also_created_dont_rename_automatically_and_dont_upload_file | ported |
| testCreateFileWithTrailingSpaces_localTrimmedAlsoCreated_dontRenameAutomaticallyAndUploadBothFiles | test_create_file_with_trailing_spaces_local_trimmed_also_created_dont_rename_automatically_and_upload_both_files | ported |
| testCreateFileWithTrailingSpaces_localAndRemoteTrimmedExists_renameFile | test_create_file_with_trailing_spaces_local_and_remote_trimmed_exists_rename_file | ported (non-Windows branch) |
| testBlockInvalidMtimeSyncRemote | test_block_invalid_mtime_sync_remote | ported |
| testBlockInvalidMtimeSyncLocal | test_block_invalid_mtime_sync_local | ported |
| testDoNotSyncInvalidFutureMtime | test_do_not_sync_invalid_future_mtime | ported |
| testInvalidFutureMtimeRecovery | test_invalid_future_mtime_recovery | ported |
| testDiscoverLockChanges | test_discover_lock_changes | ported |
| testDiscoveryUsesCorrectQuotaSource | test_discovery_uses_correct_quota_source | ported |
| testMissingInodeAndFixThem | test_missing_inode_and_fix_them | ported |

## test/testpermissions.cpp → `crates/nc-testutils/tests/testpermissions.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (test setup) |
| t7pl (+_data: move to trash, delete) | t7pl | ported |
| testForbiddenMoves | test_forbidden_moves | ported |
| testNewLocalFileInReadOnlyFolderIsKept | test_new_local_file_in_read_only_folder_is_kept | ported |
| testMoveSyncedFileIntoReadOnlyFolderRestoresContent | test_move_synced_file_into_read_only_folder_restores_content | ported |
| testParentMoveNotAllowedChildrenRestored | test_parent_move_not_allowed_children_restored | ported |
| testReadOnlyFolderIsReallyReadOnly | test_read_only_folder_is_really_read_only | ported |
| testReadWriteFolderIsReallyReadWrite | test_read_write_folder_is_really_read_write | ported |
| testChangePermissionsFolder | test_change_permissions_folder | ported |
| testChangePermissionsForFolderHierarchy | test_change_permissions_for_folder_hierarchy | ported |
| testDeleteChildItemsInReadOnlyFolder | test_delete_child_items_in_read_only_folder | ported |
| testRenameChildItemsInReadOnlyFolder | test_rename_child_items_in_read_only_folder | ported |
| testMoveChildItemsInReadOnlyFolder | test_move_child_items_in_read_only_folder | ported |
| testModifyChildItemsInReadOnlyFolder | test_modify_child_items_in_read_only_folder | ported (upstream checks `readOnlyFolder/newFolder`, which does not exist; `std::filesystem::status` then reports all bits set, so the check passes; reproduced) |
| testForbiddenDownload | test_forbidden_download | ported |
| testExistingFileBecomeForbiddenDownload | test_existing_file_become_forbidden_download | ported |
| testChangingPermissionsWithoutEtagChange | test_changing_permissions_without_etag_change | ported |
| testFolderReadonlyWhenRemotePermissionsWithoutEtagChanged (+_data, 20 rows) | test_folder_readonly_when_remote_permissions_without_etag_changed | ported |

## test/testdatabaseerror.cpp → `crates/nc-testutils/tests/testdatabaseerror.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (test setup) |
| testDatabaseError | test_database_error | ported (134 iterations of the autotest fail counter: 54 failing and recovering syncs, 80 successful) |

## test/testlockedfiles.cpp → `crates/nc-testutils/tests/testlockedfiles.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (test setup) |
| testBasicLockFileWatcher | — | n/a (GUI `LockWatcher`; the rest is Windows-only) |
| testLocalDirectoryDiscoveryReturnsAllEntries | test_local_directory_discovery_returns_all_entries | adapted (calls `discovery_single_local_directory_job` directly instead of a QThreadPool job + QSignalSpy) |
| testLockDetectionUsesRealFileSystemCheck | — | n/a (Windows-only) |
| testDirectoryLockChecks | — | n/a (Windows-only) |
| testLockedFilePropagation | — | n/a (Windows-only) |
| testPartialRecursiveRemoteRemovalNormalisesJournalPaths | — | n/a (Windows-only) |
| testPartialRecursiveRemoteRemovalDoesNotDeleteRemoteFileOnNextSync | — | n/a (Windows-only) |

## test/testlongpath.cpp → `crates/nc-testutils/tests/testlongpath.rs`

The file keeps upstream's LGPL-2.1-or-later csync header.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (test setup) |
| check_long_win_path | — | n/a (Windows-only, `FileSystem::pathtoUNC`) |
| testLongPathStat (+_data: long, long emoji, long russian, long arabic, long chinese) | test_long_path_stat | ported (against `nc_sync::filesystem::csync_vio_local_stat`) |

## test/testchunkingng.cpp → `crates/nc-testutils/tests/testchunkingng.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging / test-mode setup) |
| testChunkV2Restrictions | test_chunk_v2_restrictions | ported |
| testFileUpload | test_file_upload | ported |
| testDestinationHeaderPercentEncoding | test_destination_header_percent_encoding | ported |
| testResume1 | test_resume1 | ported |
| testResume2 | test_resume2 | ported |
| testResume3 | test_resume3 | ported |
| testResume4 | test_resume4 | ported |
| testLateAbortHard | test_late_abort_hard | adapted (`QTimer::singleShot(50, abort)` inside the override → `FakeFolder::abort_timer()`; the delayed MOVE reply is `FakeReply::Delayed` + `chunk_move_reply`) |
| testLateAbortRecoverable | test_late_abort_recoverable | adapted (idem) |
| testRemoveStale1 | test_remove_stale1 | ported |
| testRemoveStale2 | test_remove_stale2 | ported |
| testCreateConflictWhileSyncing | test_create_conflict_while_syncing | adapted (the `transmissionProgress` handler changes the remote through a `FakeServer` clone; `disconnect` is a flag) |
| testModifyLocalFileWhileUploading | test_modify_local_file_while_uploading | adapted (idem, with a `DiskFileModifier` clone) |
| testResumeServerDeletedChunks | test_resume_server_deleted_chunks | ported |
| connectionDroppedBeforeEtagRecieved (+_data: big file, small file) | connection_dropped_before_etag_recieved | adapted (`QScopedValueRollback(httpTimeout, 1)` → RAII guard over `nc_dav::jobs::set_http_timeout`) |
| testPercentEncoding | test_percent_encoding | ported |
| testVeryBigFiles | test_very_big_files | ported (2.5 GiB file, about 2 minutes) |

## test/testuploadreset.cpp → `crates/nc-testutils/tests/testuploadreset.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging / test-mode setup) |
| testFileUploadNg | test_file_upload_ng | ported |

## test/testdownload.cpp → `crates/nc-testutils/tests/testdownload.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging / test-mode setup) |
| testResume | test_resume | adapted (`BrokenFakeGetReply` = the fake GET reply with its full Content-Length but a body cut at `stopAfter`) |
| testErrorMessage | test_error_message | adapted (the 10 s safety abort is an `AbortTimer`; `timedOut` comes from `fired_count()`) |
| serverMaintenence | server_maintenence | ported |
| testMoveFailsInAConflict | test_move_fails_in_a_conflict | adapted (`touchedFile` through the `propagator_event` callback, enabled once `transmissionProgress` reports Propagation; the `QTest::qFail` in the override becomes a flag asserted after the sync) |
| testHttp2Resend | test_http2_resend | adapted (`ContentReSendError`, `Http2WasUsedAttribute` and the null status are set with a `ReplyOverride` on the fake error reply) |

*rust_only*: `rust_only_network_limits_are_enforced` (absolute download and
upload limits pace a sync through the bandwidth manager; negative limits do
not, as in v34.0.5). Upstream has no bandwidth test.

## test/testblacklist.cpp → `crates/nc-testutils/tests/testblacklist.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging / test-mode setup) |
| testBlacklistBasic (+_data: remote, local) | test_blacklist_basic | ported |

## test/syncenginetestutils.{h,cpp} (harness, no test functions)

Ported design and status per class: see the crate documentation of
`crates/nc-testutils/src/lib.rs`. Its own unit tests (`file_info::tests`,
`server::tests`, `disk::tests`, `folder::tests`, `path::tests`) are derived.

# Phase 2 daemon

## test/testinotifywatcher.cpp → `crates/nc-daemon/tests/testinotifywatcher.rs`

Upstream subclasses `FolderWatcherPrivate` to reach its protected members;
the port calls the public `FolderWatcherPrivate::default()` (no descriptor,
null parent `()`), whose inotify calls go through the `InotifySys` seam.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | `init_test_case` fixture | adapted (each test builds the tree in its own tempdir) |
| testDirsBelowPath | test_dirs_below_path | ported (including upstream's `QVERIFY(dirs.indexOf(...))` that only fails for index 0) |
| testStaleWatchDescriptorIsIgnored | test_stale_watch_descriptor_is_ignored | ported (the raw `inotify_event` is written to a `std::io::pipe` and read by `slot_received_notification`) |
| cleanupTestCase | — | n/a (tempdir cleanup is automatic) |

## test/testfolderwatcher.cpp → `crates/nc-daemon/tests/testfolderwatcher.rs`

Upstream runs the functions in order on one tree and one watcher; each Rust
test builds the fixture, runs the `init()` check, the body and the
`cleanup()` check. The shell commands (`touch`, `mkdir`, `rmdir`, `rm`, `mv`,
`echo >`) are run like upstream's `system()`. `QSignalSpy` is the event
channel of the watcher, read while the test waits.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (Qt logger / QStandardPaths test mode) |
| init | `TestFolderWatcher::init` | adapted (called by every test: clears the spy, checks the watch count) |
| cleanup | `TestFolderWatcher::cleanup` | adapted (called by every test: checks the watch count) |
| testACreate | test_a_create | ported |
| testATouch | test_a_touch | ported |
| testMove3LevelDirWithFile | test_move3_level_dir_with_file | ported |
| testCreateADir | test_create_a_dir | ported |
| testRemoveADir | test_remove_a_dir | ported |
| testRemoveAFile | test_remove_a_file | ported |
| testRenameAFile | test_rename_a_file | ported |
| testMoveAFile | test_move_a_file | ported |
| testRenameDirectorySameBase | test_rename_directory_same_base | ported |
| testRenameDirectoryDifferentBase | test_rename_directory_different_base | adapted (first replays testRenameDirectorySameBase, which created `a1/brename`) |
| testDetectLockFiles | test_detect_lock_files | ported |
| testDetectLockFilesExternally | test_detect_lock_files_externally | ported (the macOS `QSKIP` does not apply) |

*Derived* (same file): `derived_watches_exhausted_makes_unreliable_once`
(`ENOSPC` from `inotify_add_watch` → one `BecameUnreliable`),
`derived_queue_overflow_reports_lost_changes` (the `IN_Q_OVERFLOW`
divergence), `derived_journal_files_and_unknown_descriptors_are_filtered`,
`derived_notification_test` (`startNotificatonTest` with and without
notifications), `derived_set_permissions_test`.
## test/testpushnotifications.cpp → `crates/nc-dav/tests/testpushnotifications.rs`

Helpers `verifyCalledOnceWithAccount` and `failThreeAuthenticationAttempts`
are ported as `verify_called_once_with_account` and
`fail_three_authentication_attempts`. `QSignalSpy` is `SignalSpy` on the
broadcast channels of `PushNotifications::subscribe()` and
`Account::subscribe_push_notifications_events()`. The tests run on a
current-thread runtime, like the Qt event loop.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger settings and `QStandardPaths` test mode) |
| testTryReconnect_capabilitesReportPushNotificationsAvailable_reconnectForEver | test_try_reconnect_capabilites_report_push_notifications_available_reconnect_for_ever | ported |
| testSetup_correctCredentials_authenticateAndEmitReady | test_setup_correct_credentials_authenticate_and_emit_ready | ported (the spies made in `beforeAuthentication` are handed to `afterAuthentication` instead of captured) |
| testSetup_httpsAccountWithPlaintextWebSocket_doesNotSendCredentials | test_setup_https_account_with_plaintext_web_socket_does_not_send_credentials | ported (waits 200 ms before the last check, so that a wrongly opened websocket would have time to send credentials) |
| testOnWebSocketTextMessageReceived_notifyFileMessage_emitFilesChanged | test_on_web_socket_text_message_received_notify_file_message_emit_files_changed | ported |
| testOnWebSocketTextMessageReceived_notifyFileIdMessage_emitFilesChanged | test_on_web_socket_text_message_received_notify_file_id_message_emit_files_changed | ported |
| testOnWebSocketTextMessageReceived_notifyActivityMessage_emitNotification | test_on_web_socket_text_message_received_notify_activity_message_emit_notification | ported |
| testOnWebSocketTextMessageReceived_notifyNotificationMessage_emitNotification | test_on_web_socket_text_message_received_notify_notification_message_emit_notification | ported |
| testOnWebSocketTextMessageReceived_invalidCredentialsMessage_reconnectWebSocket | test_on_web_socket_text_message_received_invalid_credentials_message_reconnect_web_socket | ported |
| testOnWebSocketError_connectionLost_emitConnectionLost | test_on_web_socket_error_connection_lost_emit_connection_lost | ported |
| testSetup_maxConnectionAttemptsReached_disablePushNotifications | test_setup_max_connection_attempts_reached_disable_push_notifications | ported |
| testOnWebSocketSslError_sslError_disablePushNotifications | test_on_web_socket_ssl_error_ssl_error_disable_push_notifications | adapted (upstream emits `sslErrors` by hand on the client's `QWebSocket`; here a real `wss` fake server with a self-signed certificate fails the certificate check, so no authentication message is awaited first) |
| testAccount_web_socket_connectionLost_emitNotificationsDisabled | test_account_web_socket_connection_lost_emit_notifications_disabled | ported |
| testAccount_web_socket_authenticationFailed_emitNotificationsDisabled | test_account_web_socket_authentication_failed_emit_notifications_disabled | ported |
| testPingTimeout_pingTimedOut_reconnect | test_ping_timeout_ping_timed_out_reconnect | ported |
| — | rust_only_wss_with_trust_invalid_certificates_authenticates | rust_only (`--trust` applies to `wss`) |
| — | rust_only_account_forwards_signals_in_order | rust_only (`pushNotificationsReady` before the first `filesChanged` on the account channel) |

Derived unit test: `push_notifications::tests::derived_file_id_to_integer`
(`QJsonValue::toInteger()` on the `notify_file_id` array).

## test/pushnotificationstestutils.{h,cpp} → `crates/nc-dav/tests/pushnotificationstestutils/mod.rs` (helpers, no test functions)

* `FakeWebSocketServer` is a real websocket server (tokio-tungstenite) on
  127.0.0.1, on an ephemeral port instead of the fixed 12345 so that the
  tests can run in parallel; `create_account()` points the account at it.
  `new_secure()` adds a `wss://localhost` variant with a self-signed
  certificate (rcgen), for the certificate tests.
* `waitForTextMessages()`: `QWebSocket` delivers the user name and password
  (sent back to back) in one read, so one `QSignalSpy::wait()` sees both;
  here, after the first message, the others are collected until none
  arrives for 100 ms.
* `socketForTextMessage()` returns a `ServerSocket` handle whose
  `send_text_message()` and `abort()` (TCP closed without a closing
  handshake) act on the server-side connection.
* `CredentialsStub`: n/a, `nc_dav::Credentials` is plain data.

# Phase 2 daemon configuration

The configuration layer of `nc-daemon` (`settings`, `config_file`,
`account_config`, `folder_definition`, `credentials`, `flow2auth`,
`takeover`, `manage`) also has derived tests in each module (QSettings
escaping and a round trip of a multi-account `nextcloud.cfg` fixture,
`crates/nc-daemon/testdata/nextcloud.cfg`; ConfigFile defaults and clamps;
account loading fix-ups and migrations; folder loading rules and
migrations; credential resolution order and the QtKeychain lookup; Login
Flow v2 against a scripted transport; takeover and hand-back).

## test/testfolderman.cpp → `crates/nc-daemon/src/folder_definition/tests.rs`, `crates/nc-daemon/tests/testfolderman.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging / test-mode setup) |
| testDeleteEncryptedFiles | — | n/a (end-to-end encryption, out of scope) |
| testLeaveShare | — | n/a (`FolderMan::leaveShare`, a file manager context-menu action) |
| testCheckPathValidityForNewFolder | test_check_path_validity_for_new_folder | adapted (the configured folders are an `ExistingFolder` list; the `.sync_*.db` file upstream's `Folder` creates when it opens its journal is created by the test helper) |
| testFindGoodPathForNewSyncFolder | test_find_good_path_for_new_sync_folder | adapted (idem) |
| testProcessingFileIdsPushNotification | test_processing_file_ids_push_notification | adapted (the spied `folderSyncStateChange` emissions are read as the schedule queue, cleared before every check like upstream clears `_scheduledFolders`) |
| testUnloadAndDeleteAllFolders | test_unload_and_delete_all_folders | ported (without the socket API calls, which do not apply) |
| testMacFileProviderModeEnabledConfig | — | n/a (macOS File Provider) |
| testFileProviderEtagPollingRequiresConnectedAccount | — | n/a (macOS File Provider) |
| testAddFolderRefusedWhenFileProviderModeEnabled | — | n/a (macOS File Provider) |

## test/testforcesyncnow.cpp → `crates/nc-daemon/tests/testfolderman.rs`

`User::forceSyncNow()` (the tray's "sync now") routes to
`FolderMan::forceSyncForFolder`, which is what `ncsync sync-now` calls.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging / test-mode setup) |
| forceSyncNow_withClassicFolder_unpausesAndSchedules | force_sync_now_with_classic_folder_unpauses_and_schedules | adapted (calls `forceSyncForFolder` directly; also checks the folder is queued) |
| forceSyncNow_withoutFolderOrFileProvider_doesNotCrash | force_sync_now_without_folder_or_file_provider_does_not_crash | adapted (an unknown alias) |

## Daemon end to end (rust_only) → `crates/nc-daemon/tests/rust_only_daemon.rs`

`rust_only_daemon_polls_watches_and_obeys_control_requests`: the event
loop against the FakeFolder server: the initial sync of a loaded folder, a
remote change found by etag polling, a local change found by inotify,
pause (a remote change is not applied), resume and sync-now.

`rust_only_daemon_selective_sync_and_folder_removal`: the `selective-sync`
control request (`FolderStatusModel::slotApplySelectiveSync`): an excluded
folder loses its local files unchanged since the last sync and keeps a
changed one, the server keeps everything, a file or a subfolder of an
excluded folder is refused, an included folder is downloaded again and goes
to the white list; then a reload unloads the folder and keeps its journal,
closed. `rust_only_daemon_removed_folder_journal_wiped_after_its_sync`: a
folder removed by a reload while a download hangs: the answer waits for the
aborted engine, then the journal is gone and stays gone.

## test/testaccountmanager.cpp → `crates/nc-daemon/src/account_config/tests.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase, cleanup | — | n/a (test setup) |
| testAccountFromUserId_asciiDomain_matchesByExactId | — | n/a (`accountFromUserId` serves the `nc://` URI handler and the File Provider, not the daemon) |
| testAccountFromUserId_unknownId_returnsNull | — | n/a (idem) |
| testAccountFromUserId_idnDomain (+_data) | — | n/a (idem) |
| testAccountFromUserId_idnDomainWithPort (+_data) | — | n/a (idem) |
| restoresPublicShareLinkWithBasicCredentials | restores_public_share_link_with_basic_credentials | ported (on the loaded `AccountDefinition`: auth type `http`, public share link, login name) |

## test/testaccount.cpp → `crates/nc-daemon/src/account_config/tests.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logging setup) |
| testAccountDavPath_unitialized_noCrash | — | n/a (`nc_dav::Account` is always built with its URL) |
| testAccount_isPublicShareLink (+_data, 8 rows) | test_account_is_public_share_link | adapted (`Account::setUrl`'s detection is `public_share_link_parts`, used when an account is loaded) |
| testAccount_setLimitSettings_globalNetworkLimitFallback | test_account_set_limit_settings_global_network_limit_fallback | adapted (the setters are on `AccountDefinition`) |
| testAccount_listRemoteFolder (+_data) | — | n/a (`Account::listRemoteFolder` serves the GUI folder wizard) |

# Phase 3

## test/testsyncfilestatustracker.cpp → `crates/nc-testutils/tests/testsyncfilestatustracker.rs`

The tracker (`SyncFileStatusTracker`, `SyncFileStatus`) is in
`crates/nc-sync/src/sync_file_status_tracker.rs` and `sync_file_status.rs`;
the engine owns it and calls its slots before its own callbacks. Upstream
pauses the sync (`scheduleSync()`, `execUntilBeforePropagation()`,
`execUntilItemCompleted(path)`, `execUntilFinished()`); the port's sync is
one future borrowing the engine, so the checks of a pause point run in the
engine's `about_to_propagate` / `item_completed` callbacks at that point
(adapted). `QEXPECT_FAIL` comparisons become `assert_ne!` (an unexpected
pass fails upstream too).

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger / QStandardPaths test mode) |
| parentsGetSyncStatusUploadDownload | parents_get_sync_status_upload_download | adapted (pause point in `about_to_propagate`) |
| parentsGetSyncStatusNewFileUploadDownload | parents_get_sync_status_new_file_upload_download | adapted (idem) |
| parentsGetSyncStatusNewDirDownload | parents_get_sync_status_new_dir_download | adapted (idem, and `item_completed` of `D`) |
| parentsGetSyncStatusNewDirUpload | parents_get_sync_status_new_dir_upload | adapted (idem) |
| parentsGetSyncStatusDeleteUpDown | parents_get_sync_status_delete_up_down | adapted (pause point in `about_to_propagate`) |
| warningStatusForExcludedFile | warning_status_for_excluded_file | adapted (idem; 3 `QEXPECT_FAIL`) |
| warningStatusForExcludedFile_CasePreserving | warning_status_for_excluded_file_case_preserving | ported |
| parentsGetWarningStatusForError | parents_get_warning_status_for_error | adapted (pause points in `about_to_propagate`) |
| parentsGetWarningStatusForError_SibblingStartsWithPath | parents_get_warning_status_for_error_sibbling_starts_with_path | adapted (idem) |
| childOKEmittedBeforeParent | child_ok_emitted_before_parent | ported |
| sharedStatus | shared_status | adapted (pause point in `about_to_propagate`; 1 `QEXPECT_FAIL`) |
| renameError | rename_error | adapted (idem) |
| silentlyExcludedFilesRemovedFromExclude | silently_excluded_files_removed_from_exclude | ported |

derived: `sync_file_status::tests::derived_socket_api_string`,
`sync_file_status_tracker::tests::derived_lookup_problem`,
`propagator::put_multi_file::tests::derived_multipart_format`.

# Phase 3 file locking

## test/testlockfile.cpp → `crates/nc-testutils/tests/testlockfile.rs`

GPL-2.0-or-later like the upstream file. `QSignalSpy::wait()` on the
account's `lockFileSuccess` / `lockFileError` or on the job's
`finishedWithoutError` / `finishedWithError` is running the request to its
end: `nc_sync::account_lock::set_lock_file_state` and `LockFileJob::start`
resolve to `Ok` or `Err`. `SyncEngine::lockFileDetected` is the engine
callback `lock_file_detected`.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (Qt logger / QStandardPaths test mode) |
| testLockFile_lockFile_lockSuccess | test_lock_file_lock_file_lock_success | ported |
| testLockFile_lockFile_lockError | test_lock_file_lock_file_lock_error | ported |
| testLockFile_fileLockStatus_queryLockStatus | test_lock_file_file_lock_status_query_lock_status | ported |
| testLockFile_fileCanBeUnlocked_canUnlock | test_lock_file_file_can_be_unlocked_can_unlock | ported |
| testLockFile_lockFile_jobSuccess | test_lock_file_lock_file_job_success | ported |
| testLockFile_lockFile_unlockFile_jobSuccess | test_lock_file_lock_file_unlock_file_job_success | ported |
| testLockFile_lockFile_alreadyLockedByUser | test_lock_file_lock_file_already_locked_by_user | ported |
| testLockFile_lockFile_alreadyLockedByApp | test_lock_file_lock_file_already_locked_by_app | ported |
| testLockFile_unlockFile_alreadyUnlocked | test_lock_file_unlock_file_already_unlocked | ported |
| testLockFile_unlockFile_lockedBySomeoneElse | test_lock_file_unlock_file_locked_by_someone_else | ported |
| testLockFile_lockFile_jobError | test_lock_file_lock_file_job_error | ported |
| testLockFile_lockFile_preconditionFailedError | test_lock_file_lock_file_precondition_failed_error | ported |
| testSyncLockedFilesAlmostExpired | test_sync_locked_files_almost_expired | adapted (the engine's scheduled sync run timers are deadlines: `spySyncCompleted.wait(4000)` is "the next deadline is within 4 s", then `fire_due` and a sync run; same real-time waits) |
| testSyncLockedFilesNoExpiredLockedFiles | test_sync_locked_files_no_expired_locked_files | adapted (idem: no deadline within 3 s, nothing fires) |
| testSyncLockedFiles | test_sync_locked_files | ported |
| testLockFile_lockedFileReadOnly_afterSync | test_lock_file_locked_file_read_only_after_sync | ported |
| testLockFile_lockFile_detect_newly_uploaded | test_lock_file_lock_file_detect_newly_uploaded | ported |
| testLockFile_lockFile_detect_newly_uploaded_autocad | test_lock_file_lock_file_detect_newly_uploaded_autocad | ported |
| testLockFile_lockFile_detect_newly_uploaded_adobe_idlk | test_lock_file_lock_file_detect_newly_uploaded_adobe_idlk | ported |
| testLockFile_autoCADLockFileTargetFilePath_resolution | test_lock_file_auto_cad_lock_file_target_file_path_resolution | ported |
| testLockFile_lockFile_detect_newly_uploaded_adobe_prlock | test_lock_file_lock_file_detect_newly_uploaded_adobe_prlock | ported |
| testLockFile_adobeLockFileTargetFilePath_resolution | test_lock_file_adobe_lock_file_target_file_path_resolution | ported |
| testLockFile_lockFile_detect_newly_uploaded_affinity_afphoto | test_lock_file_lock_file_detect_newly_uploaded_affinity_afphoto | ported |
| testLockFile_lockFile_detect_newly_uploaded_affinity_afdesign | test_lock_file_lock_file_detect_newly_uploaded_affinity_afdesign | ported |
| testLockFile_affinityLockFileTargetFilePath_resolution | test_lock_file_affinity_lock_file_target_file_path_resolution | ported (in a temporary directory instead of the fixed `/tmp/affinityTestDir/`) |
| testLockFile_verifyE2eeFilesUseCorrectPath | test_lock_file_verify_e2ee_files_use_correct_path | ported (no encryption involved: the journal record carries a mangled name, which `LockFileJob` must use in the URL) |
| testUploadLockedFilesInDeletedFolder | test_upload_locked_files_in_deleted_folder | ported (the override's `Q_ASSERT(false)` on an `If` header is a flag checked at the end) |

The lock file helpers (`src/libsync/filesystem.cpp`, now in
`crates/nc-sync/src/filesystem/lock_file.rs`) also keep their *derived*
unit tests there. *rust_only* in `crates/nc-daemon/tests/rust_only_daemon.rs`:
`rust_only_daemon_locks_documents_opened_in_an_office_application` (a
LibreOffice lock file appearing and disappearing next to a synced document
locks then unlocks it on the server, through the inotify watcher and
`Folder::slotLockedFilesFound` / `slotFilesLockReleased`).

## test/testfilesystem.cpp → `crates/nc-sync/tests/testfilesystem.rs`

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (Qt logger / test mode; each test creates its directory) |
| testSetFolderPermissionsExistingDirectory (+_data, 6 rows) | test_set_folder_permissions_existing_directory | ported |
| testSetFolderPermissionsNonexistentDirectory | test_set_folder_permissions_nonexistent_directory | ported |
| testSetFileReadOnlyLongPath | test_set_file_read_only_long_path | ported (the temporary file is created in a temporary directory, not in the working directory) |
| testRenameFileWithTrailingPeriod | — | n/a (Windows-only) |
| testAclWithManyDeniedAces | — | n/a (Windows-only, ACLs) |
| testRecursiveDeletionLongPaths | test_recursive_deletion_long_paths | ported |

## test/testnextcloudcmdprovisioning.cpp → `crates/ncsync/tests/testnextcloudcmdprovisioning.rs`

`nextcloudcmd ARGS` is `ncsync sync ARGS`, run as a child process
(`CARGO_BIN_EXE_ncsync`) with stdin closed, like upstream's `QProcess`.
Every run gets its own `HOME` and XDG directories and an unreachable
D-Bus session (upstream's runs without `--confdir` use the configuration
of the user running the tests), so nothing reaches the user's
configuration or keyring. Upstream's `return -1` is exit code 255 on Linux,
as the upstream test expects there.

| Upstream | Rust | Status |
|---|---|---|
| testUserIdAlonePrintsServerUrlError | test_user_id_alone_prints_server_url_error | ported |
| testUserIdAndServerUrlWithoutAppPasswordEntersProvisionMode | test_user_id_and_server_url_without_app_password_enters_provision_mode | ported (the quoted `"http://127.0.0.1:1"` is an invalid `QUrl`: 255 from "Missing mandatory command line options") |
| testProvisioningOptionsEnterProvisionModeNotSyncMode | test_provisioning_options_enter_provision_mode_not_sync_mode | ported |
| testNoArgsShowsHelp | test_no_args_shows_help | adapted (`ncsync sync` without arguments prints the clap help of `ncsync sync`, which names `ncsync` instead of `nextcloudcmd`, and exits 0) |
| testNonInteractiveFlagDoesNotSuppressProvisioningError | test_non_interactive_flag_does_not_suppress_provisioning_error | ported |
| testInlineOptionValuesSelectProvisioningMode | test_inline_option_values_select_provisioning_mode | ported |
| testInlineOptionValuesReachAccountSetup | test_inline_option_values_reach_account_setup | ported |
| testSetupWithoutLocalDirPathIsNotRejected | test_setup_without_local_dir_path_is_not_rejected | ported |
| testAppPasswordSetupRunsTheEventLoop | test_app_password_setup_runs_the_event_loop | ported |

rust_only (same file, against a local `TcpListener` server,
`nc-testutils/tests/common/http_server.rs`):
`rust_only_provisioning_writes_account_folder_and_app_password` (the
account, the folder `0` with the remote path, the password file without a
keyring, the folder's journal, then "Account alice already exists!" with
255), `rust_only_provisioning_refuses_virtual_files_and_writes_nothing`,
`rust_only_provisioning_wrong_app_password_writes_nothing` (no account,
no local folder),
`rust_only_provisioning_non_empty_local_folder_is_rejected`,
`rust_only_provisioning_unknown_option_shows_help` (`HelpMode`, exit 0).
For the three documented divergences (README, "Divergences from upstream"):
without an app password, Login Flow v2 instead of an account stored
without credentials —
`rust_only_provisioning_without_app_password_logs_in_with_login_flow_v2`
(the link printed with `?user=alice`, `login/v2` then the poll then
`ocs/v1.php/cloud/user`, the account and the app password stored),
`rust_only_provisioning_non_interactive_without_app_password_is_refused`
(255, an error naming `--apppassword`, no request sent, nothing written),
`rust_only_provisioning_login_flow_as_another_user_writes_nothing` (the
browser logged in as another user: 1, nothing written),
`rust_only_provisioning_login_flow_unreachable_server_writes_nothing`;
`--trust` applied to the setup —
`rust_only_provisioning_trust_accepts_an_invalid_certificate` (a TLS
server with a self-signed certificate: 1 and nothing written without
`--trust`, 0 with it);
`--httpproxy` applied to the setup —
`rust_only_provisioning_httpproxy_carries_every_setup_request` (a local
forwarding proxy: with `--httpproxy` it carries every request the server
gets, `login/v2` and its poll included; without it, none).
Derived unit tests: `account_setup::tests::derived_qurl_validity`,
`derived_display_name`.

## test/testremotewipe.cpp → `crates/nc-daemon/tests/testremotewipe.rs`

Upstream swaps the `RemoteWipe`'s own `QNetworkAccessManager` for a
`FakeQNAM` sharing the override; here the wipe requests go through the
account's transport (the FakeFolder server, without credentials, like that
separate manager), so one override answers everything. `QTest::qWait(500)`
handles the folder manager's queue for 500 ms.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | — | n/a (logger / `QStandardPaths` test mode) |
| testRemoteWipe | test_remote_wipe | adapted (upstream's keychain gives an empty app password in tests, so the 401 of the sync makes no check; here the 401 also starts a check with the account's stored password, answered 401 as well; both phases then call `startCheckJobWithAppPassword("password")` like upstream; also checks the account was signed out, its settings removal, and that `wipe/check` then `wipe/success` were called) |

rust_only: `rust_only_remote_wipe_without_folder_keeps_the_account`
(upstream's `wipeDone(account, false)` when the account has no folder: the
account stays and the server is not told),
`rust_only_remote_wipe_empty_app_password_does_not_check`. Derived unit
test: `remote_wipe::tests::derived_token_encoding`.

## test/testclientstatusreporting.cpp → `crates/nc-testutils/tests/testclientstatusreporting.rs`

The upstream file is GPL-2.0-or-later and so is the port. Upstream's
functions share one account and database; here each test builds the
fixture.

| Upstream | Rust | Status |
|---|---|---|
| initTestCase | `init_test_case` | adapted (called by every test: the 1 s / 2 s intervals, an account on the fake server whose override records the PUT body, the database in a temporary directory instead of `dbPathForTesting`, the `security_guard.diagnostics` capability) |
| testReportAndSendStatuses | test_report_and_send_statuses | ported |
| testNothingReportedAndNothingSent | test_nothing_reported_and_nothing_sent | ported (on a fresh fixture, where upstream runs after the previous test emptied the database) |
| cleanupTestCase | `cleanup_test_case` | adapted (called by every test) |

rust_only: `rust_only_reporting_follows_the_capability` (the database is
created with the reporting; a capability switched off drops it),
`rust_only_sync_engine_reports_item_errors` (`PropagateItemJob::reportClientStatuses`
on a FakeFolder sync: a failed upload and a failed download).

## Not applicable

| Upstream | Rust | Status |
|---|---|---|
| test/testcookies.cpp testCookies | — | n/a (`CookieJar::save`/`restore` persist cookies to `cookies<id>.db` for the GUI's `AccountManager`; `nextcloudcmd` keeps them in memory per process and so do `ncsync` and `ncsyncd` (reqwest's jar, which cannot be exported). With an app password the server's persistent cookies are only its `__Host-nc_sameSiteCookie*` markers, so nothing that the persistence would save matters for the sync) |
| test/testconnectionvalidator.cpp initTestCase | — | n/a (test setup) |
| test/testconnectionvalidator.cpp localNetworkPermissionFailureReplacesTimeout | — | n/a (macOS local network permission: on Linux `LocalNetworkPermission::checkDeniedForConnection` always answers "not denied", so the timeout message is never replaced) |
| test/testfolder.cpp initTestCase | — | n/a (test setup) |
| test/testfolder.cpp test_sidebarDisplayName | — | n/a (`Folder::sidebarDisplayName` names the folder in the Windows Explorer navigation pane and the virtual files providers; no Linux sync path uses it) |


# Side-by-side bench against nextcloudcmd v34.0.5

`tools/bench/bench.py` (see the README) runs 19 scripted steps with the
official `nextcloudcmd` built from the pinned tag (`tools/bench/build-oracle.sh`,
upstream CI image) and with `ncsync`, each against its own user of the test
server: initial up/download, local edits/deletes/moves/folder renames,
remote edits/moves/deletes/folder moves, chunked upload, conflicts (both
edited, same content, delete vs edit both ways, new on both sides), a
directory/file type conflict, renames on both sides, case-differing names,
mtime-only changes, NFC/NFD, leading/trailing spaces and non-Latin names,
150 local + 40 remote files, a folder created on both sides, remote type
changes, a case-only folder rename, deletions on both sides, idle syncs.
After every step it compares the local trees (names, sizes, contents,
mtimes, modes), the server trees and a dump of the journals (schema, every
table; etags, file ids and inodes as "matches the server/disk").

`--roundtrip` alternates the two clients on one folder and journal, and
after every step has the other client take the folder over: it must find
nothing to do, and the result must equal the nextcloudcmd-only run.

Differences found and fixed:

| Found | Cause | Fix |
|---|---|---|
| `metadata.e2eCertificateFingerprint` is `''` upstream, NULL here | `setFileRecord` binds it with `bindValue(19, {})`, i.e. an empty `QByteArray`: TEXT `''` | bound as `''` (`nc-journal`, 1ab49af) |
| testsyncmove/testMovePropagation (pending) | upstream's `QCOMPARE` of `printDbData` goes through `QTest::toString`, truncated to 245 characters (shown by the upstream test binary) | the truncation is reproduced in `print_db_data` (c9bb556) |

Bench artefacts (not client differences), normalized by the script: the
`last_sync` time, the creation time of directories, the conflict date in
file names, the umask (the oracle's container uses 022).

Last result (2026-10-08): side by side 0 differences in 19 steps;
round trip: the other client never had anything to do after a takeover
(19/19). Compared with the nextcloudcmd-only run, one run showed
`quotaBytesUsed` 0 instead of 288002 for one folder for 6 steps; it did
not reproduce (3 more runs of each client, and the same sequence again,
all 288002): both clients copy the server's `quota-used-bytes` of the
PROPFIND, which the server had not updated yet at that moment.
