// SPDX-FileCopyrightText: 2025 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testfilesystem.cpp (nextcloud/desktop v34.0.5).
//
// Not ported: `testRenameFileWithTrailingPeriod` and
// `testAclWithManyDeniedAces` are Windows-only upstream (`#ifdef Q_OS_WIN`:
// long Windows paths, ACLs).

use std::fs;
use std::os::unix::fs::PermissionsExt;

use nc_sync::filesystem::{
    self, FolderPermissions, set_file_read_only, set_folder_permissions_changed,
};

// initTestCase: creates `existingDirectory` in the shared `testDir`; each
// test below creates what it needs.

fn permissions(path: &str) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

#[test]
fn test_set_folder_permissions_existing_directory() {
    let test_dir = tempfile::tempdir().expect("temporary directory");
    let full_path = format!("{}/existingDirectory", test_dir.path().to_str().unwrap());
    fs::create_dir(&full_path).expect("mkdir");

    const PERMS_0555: u32 = 0o555;
    const PERMS_0755: u32 = PERMS_0555 | 0o200;
    const PERMS_0775: u32 = PERMS_0755 | 0o020;
    const ALL: u32 = 0o777;

    // (name, originalPermissions, folderPermissions, expectedResult,
    // expectedPermissionsChanged, expectedPermissions)
    let rows = [
        (
            "0777, readonly -> 0555, changed",
            ALL,
            FolderPermissions::ReadOnly,
            true,
            true,
            PERMS_0555,
        ),
        (
            "0555, readonly -> 0555, not changed",
            PERMS_0555,
            FolderPermissions::ReadOnly,
            true,
            false,
            PERMS_0555,
        ),
        (
            "0777, readwrite -> 0775, changed",
            ALL,
            FolderPermissions::ReadWrite,
            true,
            true,
            PERMS_0775,
        ),
        (
            "0775, readwrite -> 0775, not changed",
            PERMS_0775,
            FolderPermissions::ReadWrite,
            true,
            false,
            PERMS_0775,
        ),
        (
            "0755, readwrite -> 0755, not changed",
            PERMS_0755,
            FolderPermissions::ReadWrite,
            true,
            false,
            PERMS_0755,
        ),
        (
            "0555, readwrite -> 0755, changed",
            PERMS_0555,
            FolderPermissions::ReadWrite,
            true,
            true,
            PERMS_0755,
        ),
    ];

    for (
        name,
        original_permissions,
        folder_permissions,
        expected_result,
        expected_permissions_changed,
        expected_permissions,
    ) in rows
    {
        let mut permissions_did_change = false;

        fs::set_permissions(&full_path, fs::Permissions::from_mode(original_permissions))
            .expect("chmod");

        assert_eq!(
            set_folder_permissions_changed(
                &full_path,
                folder_permissions,
                &mut permissions_did_change
            ),
            expected_result,
            "{name}"
        );

        let new_permissions = permissions(&full_path);
        assert_eq!(new_permissions, expected_permissions, "{name}");
        assert_eq!(
            permissions_did_change, expected_permissions_changed,
            "{name}"
        );
    }

    // Let the temporary directory be removed.
    fs::set_permissions(&full_path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn test_set_folder_permissions_nonexistent_directory() {
    let mut permissions_did_change = false;

    // Like upstream, the relative name is passed, not the full path.
    assert!(!set_folder_permissions_changed(
        "nonexistentDirectory",
        FolderPermissions::ReadOnly,
        &mut permissions_did_change
    ));
    assert!(!permissions_did_change);
}

#[test]
fn test_set_file_read_only_long_path() {
    // this should be enough to hit Windows' default MAX_PATH limitation (260 chars)
    // (QTemporaryFile with a relative template creates the file in the
    // working directory; it is created in a temporary directory here.)
    let dir = tempfile::tempdir().expect("temporary directory");
    let long_file = tempfile::Builder::new()
        .prefix(&"test".repeat(60))
        .tempfile_in(dir.path())
        .expect("create the temporary file");

    let path = long_file.path().to_str().unwrap().to_owned();
    assert!(!path.is_empty());

    // ensure we start with a read/writeable directory
    assert!(filesystem::is_writable(&path));

    set_file_read_only(&path, true);
    // path should now be readonly
    assert!(!filesystem::is_writable(&path));

    set_file_read_only(&path, false);
    // path should now be writable aagain
    assert!(filesystem::is_writable(&path));
}

#[test]
fn test_recursive_deletion_long_paths() {
    let temp_dir = tempfile::tempdir().expect("temporary directory");
    let root = temp_dir.path().to_str().unwrap().to_owned();

    let make_long_path_segment = |base: &str| -> String {
        let base_segment = format!("{base} ");
        let pad = 200 - base_segment.chars().count();
        format!("{base_segment}{}", "0".repeat(pad))
    };

    assert!(filesystem::mkpath(&format!(
        "{root}/Folder 1 - short name but long single subfolder/{}",
        make_long_path_segment("Subfolder without accentued characters - padding")
    )));
    assert!(filesystem::mkpath(&format!(
        "{root}/{}",
        make_long_path_segment("Folder 2 - directly a very long name")
    )));
    let mut folder3_path = "Folder 3 - short name but long multiples subfolder".to_owned();
    for i in 0..5 {
        folder3_path.push_str(&format!(
            "/subfolder {} - total length 40 characters",
            i + 1
        ));
    }
    assert!(filesystem::mkpath(&format!("{root}/{folder3_path}")));
    assert!(filesystem::mkpath(&format!("{root}/Folder 4 - short name")));

    let mut errors = Vec::new();
    assert!(filesystem::remove_recursively(
        &root,
        &mut |_, _| {},
        &mut errors
    ));
    assert!(errors.is_empty(), "{errors:?}");
}
