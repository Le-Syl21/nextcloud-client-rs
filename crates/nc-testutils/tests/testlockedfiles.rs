// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2018 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testlockedfiles.cpp (nextcloud/desktop v34.0.5).
//
// Not ported: `testBasicLockFileWatcher` tests the GUI `LockWatcher`
// (src/gui), which has no counterpart in the port; every other test is
// Windows-only upstream (`#ifdef Q_OS_WIN`: file handles opened without
// sharing by another process).

use nc_sync::discovery::{LocalListingResult, discovery_single_local_directory_job};

// Functional check for local directory discovery #10535: DiscoverySingleLocalDirectoryJob
// must return every regular file and subdirectory with its name and flags intact.
#[test]
fn test_local_directory_discovery_returns_all_entries() {
    let tmp = tempfile::tempdir().expect("temporary directory");
    let mut expected_files = Vec::new();
    for i in 0..50 {
        // Varied lengths and non ascii, matching the discovery concat path.
        let name = format!("entry_{i}_ααβγ_{}.txt", "x".repeat(i % 20));
        std::fs::write(tmp.path().join(&name), b"data").expect("write file");
        expected_files.push(name);
    }
    assert!(std::fs::create_dir(tmp.path().join("subdir")).is_ok());

    let mut undecodable = 0;
    let result = discovery_single_local_directory_job(tmp.path().to_str().unwrap(), false, |_| {
        undecodable += 1
    });
    assert_eq!(undecodable, 0);
    let LocalListingResult::Finished(results) = result else {
        panic!("the local listing failed: {result:?}");
    };
    assert_eq!(results.len(), expected_files.len() + 1);

    let mut seen_files = Vec::new();
    for info in &results {
        assert!(!info.name.is_empty());
        if info.is_directory {
            assert_eq!(info.name, "subdir");
            continue;
        }
        assert!(!info.is_locked);
        seen_files.push(info.name.clone());
    }
    seen_files.sort();
    expected_files.sort();
    assert_eq!(seen_files, expected_files);
}
