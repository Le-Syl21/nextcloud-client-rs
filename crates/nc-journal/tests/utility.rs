// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud, Inc.
// SPDX-FileCopyrightText: 2013 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// Port of the tests of upstream `test/testsyncconflict.cpp` and
// `test/testutility.cpp` (nextcloud/desktop v34.0.5) that cover the ported
// parts of `src/common/utility.cpp`:
// - testConflictFileBaseName (+ its _data rows), from testsyncconflict.cpp;
// - testFsCasePreserving, from testutility.cpp.
// The FakeFolder-based tests of testsyncconflict.cpp belong to Phase 1.

use nc_journal::utility::{
    conflict_file_base_name_from_pattern, fs_case_preserving, is_mac, is_windows,
    set_fs_case_preserving_override,
};

#[test]
fn test_fs_case_preserving() {
    // Upstream assumes OWNCLOUD_TEST_CASE_PRESERVING is not set.
    if std::env::var_os("OWNCLOUD_TEST_CASE_PRESERVING").is_none() {
        assert!(if is_mac() || is_windows() {
            fs_case_preserving()
        } else {
            !fs_case_preserving()
        });
    }
    set_fs_case_preserving_override(Some(true));
    assert!(fs_case_preserving());
    set_fs_case_preserving_override(Some(false));
    assert!(!fs_case_preserving());
    // QScopedValueRollback
    set_fs_case_preserving_override(None);
}

#[test]
fn test_conflict_file_base_name() {
    let rows: &[(&str, &str, &str)] = &[
        ("nomatch1", "a/b/foo", ""),
        ("nomatch2", "a/b/foo.txt", ""),
        ("nomatch3", "a/b/foo_conflict", ""),
        ("nomatch4", "a/b/foo_conflict.txt", ""),
        ("match1", "a/b/foo_conflict-123.txt", "a/b/foo.txt"),
        ("match2", "a/b/foo_conflict-foo-123.txt", "a/b/foo.txt"),
        ("match3", "a/b/foo_conflict-123", "a/b/foo"),
        ("match4", "a/b/foo_conflict-foo-123", "a/b/foo"),
        // new style
        (
            "newmatch1",
            "a/b/foo (conflicted copy 123).txt",
            "a/b/foo.txt",
        ),
        (
            "newmatch2",
            "a/b/foo (conflicted copy foo 123).txt",
            "a/b/foo.txt",
        ),
        ("newmatch3", "a/b/foo (conflicted copy 123)", "a/b/foo"),
        ("newmatch4", "a/b/foo (conflicted copy foo 123)", "a/b/foo"),
        (
            "newmatch5",
            "a/b/foo (conflicted copy foo 123) bla",
            "a/b/foo bla",
        ),
        (
            "newmatch6",
            "a/b/foo (conflicted copy foo.bar 123)",
            "a/b/foo",
        ),
        // double conflict files
        (
            "double1",
            "a/b/foo_conflict-123_conflict-456.txt",
            "a/b/foo_conflict-123.txt",
        ),
        (
            "double2",
            "a/b/foo_conflict-foo-123_conflict-bar-456.txt",
            "a/b/foo_conflict-foo-123.txt",
        ),
        (
            "double3",
            "a/b/foo (conflicted copy 123) (conflicted copy 456).txt",
            "a/b/foo (conflicted copy 123).txt",
        ),
        (
            "double4",
            "a/b/foo (conflicted copy 123)_conflict-456.txt",
            "a/b/foo (conflicted copy 123).txt",
        ),
        (
            "double5",
            "a/b/foo_conflict-123 (conflicted copy 456).txt",
            "a/b/foo_conflict-123.txt",
        ),
    ];
    for (name, input, output) in rows {
        assert_eq!(
            String::from_utf8(conflict_file_base_name_from_pattern(input.as_bytes())).unwrap(),
            *output,
            "row {name}"
        );
    }
}
