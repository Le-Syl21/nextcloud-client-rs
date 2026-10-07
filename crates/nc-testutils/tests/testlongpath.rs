// libcsync -- a library to sync a directory with another
//
// SPDX-FileCopyrightText: 2024 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud, Inc.
// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Port of upstream test/testlongpath.cpp (nextcloud/desktop v34.0.5).
//
// `check_long_win_path` (FileSystem::pathtoUNC) is Windows-only upstream
// (`#ifdef Q_OS_WIN`) and not ported.

use nc_sync::filesystem::csync_vio_local_stat;

const LONG_PREFIX: &str = "/alonglonglonglong/blonglonglonglong/clonglonglonglong/dlonglonglonglong/\
elonglonglonglong/flonglonglonglong/glonglonglonglong/hlonglonglonglong/ilonglonglonglong/\
jlonglonglonglong/klonglonglonglong/llonglonglonglong/mlonglonglonglong/nlonglonglonglong/\
olonglonglonglong/";

#[test]
fn test_long_path_stat() {
    let rows = [
        ("long", "file.txt"),
        ("long emoji", "file🐷.txt"),
        ("long russian", "собственное.txt"),
        ("long arabic", "السحاب.txt"),
        ("long chinese", "自己的云.txt"),
    ];
    for (row, file_name) in rows {
        // _data row `row`
        let tmp = tempfile::tempdir().expect("temporary directory");
        let name = format!("{LONG_PREFIX}{file_name}");
        let long_path = format!("{}{}", tmp.path().to_str().unwrap(), name);

        let data = b"hello";
        eprintln!("{row}: {long_path}");
        let dir = std::path::Path::new(&long_path).parent().unwrap();
        assert!(std::fs::create_dir_all(dir).is_ok(), "{row}");

        assert!(std::fs::write(&long_path, data).is_ok(), "{row}");

        let buf = csync_vio_local_stat(&long_path, true);
        assert!(buf.is_some(), "{row}");
        let buf = buf.unwrap();
        assert!(buf.size == data.len() as i64, "{row}");
        assert!(
            buf.size == std::fs::metadata(&long_path).unwrap().len() as i64,
            "{row}"
        );

        assert!(tmp.close().is_ok(), "{row}");
    }
}
