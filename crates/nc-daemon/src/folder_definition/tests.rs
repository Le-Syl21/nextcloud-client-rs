// SPDX-FileCopyrightText: 2019 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// Port of upstream test/testfolderman.cpp (testCheckPathValidityForNewFolder,
// testFindGoodPathForNewSyncFolder; nextcloud/desktop v34.0.5), plus derived
// tests of the folder definitions.

use super::*;
use crate::account_config::load_accounts;

const FIXTURE: &str = include_str!("../../testdata/nextcloud.cfg");

#[test]
fn derived_escape_alias_round_trip() {
    let alias = "a/b\\c?d%e*f:g|h\"i<j>k[l]m";
    let escaped = escape_alias(alias);
    assert_eq!(
        escaped,
        "a__SLASH__b__BSLASH__c__QMARK__d__PERCENT__e__STAR__f__COLON__g__PIPE__h__QUOTE__i__LESS_THAN__j__GREATER_THAN__k__PAR_OPEN__l__PAR_CLOSE__m"
    );
    assert_eq!(unescape_alias(&escaped), alias);
    assert_eq!(escape_alias("1"), "1");
}

#[test]
fn derived_prepare_paths() {
    assert_eq!(FolderDefinition::prepare_local_path("/a/b"), "/a/b/");
    assert_eq!(FolderDefinition::prepare_local_path("/a/b/"), "/a/b/");
    assert_eq!(FolderDefinition::prepare_target_path(""), "/");
    assert_eq!(FolderDefinition::prepare_target_path("/"), "/");
    assert_eq!(
        FolderDefinition::prepare_target_path("foo/bar/"),
        "/foo/bar"
    );
    let d = FolderDefinition {
        local_path: "/l/".into(),
        journal_path: ".sync_x.db".into(),
        ..Default::default()
    };
    assert_eq!(d.absolute_journal_path(), "/l/.sync_x.db");
}

#[test]
fn derived_load_fixture_folders() {
    let s = Settings::parse(FIXTURE).unwrap();
    let accounts = load_accounts(&s).accounts;
    let load = load_folders(&s, &accounts);
    let summary: Vec<_> = load
        .folders
        .iter()
        .map(|f| (f.account_id.as_str(), f.group, f.definition.alias.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            ("0", FolderGroup::Folders, "1"),
            ("0", FolderGroup::Folders, "Docs/Work"),
            ("1", FolderGroup::Multifolders, "2"),
            ("1", FolderGroup::FoldersWithPlaceholders, "1"),
        ]
    );
    let docs = &load.folders[1].definition;
    assert_eq!(docs.local_path, "/home/alice/My Docs, 2024/");
    assert_eq!(docs.target_path, "/Documents/Work");
    assert_eq!(docs.journal_path, ".sync_0a1b2c3d4e5f.db");
    assert!(docs.paused);
    assert!(!docs.ignore_hidden_files);
    assert!(load.folders[0].save_backwards_compatible);
    assert_eq!(
        load.folders[0].journal_migration,
        JournalMigration::MaybeMigrateLegacyDb
    );
    assert!(load.folders[2].definition.check_supported().is_ok());
    let vfs = &load.folders[3];
    assert!(vfs.save_in_folders_with_placeholders);
    assert_eq!(vfs.definition.virtual_files_mode, VfsMode::WithSuffix);
    assert!(matches!(
        vfs.definition.check_supported(),
        Err(FolderError::VirtualFilesNotSupported { mode: "suffix", .. })
    ));
}

#[test]
fn derived_load_rules_and_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let l = dir.path().to_string_lossy().into_owned();
    let text = format!(
        "[Accounts]\n0\\url=https://h\n0\\authType=webflow\n0\\webflow_user=u\n\
         0\\Folders\\a\\localPath={l}\n0\\Folders\\a\\targetPath=remote/\n\
         0\\Folders\\b\\localPath={l}/b\n0\\Folders\\b\\journalPath=/var/old/journal.db\n\
         0\\Folders\\c\\localPath={l}/c\n0\\Folders\\c\\journalPath=._sync_123.db\n\
         0\\Folders\\d\\localPath={l}/d\n0\\Folders\\d\\version=14\n\
         0\\Folders\\e\\localPath={l}/e\n0\\Folders\\e\\usePlaceholders=true\n\
         0\\Folders\\f\\localPath={l}/f\n0\\Folders\\f\\virtualFilesMode=xattr\n\
         0\\Multifolders\\version=2\n0\\Multifolders\\g\\localPath={l}/g\n\
         version=13\n"
    );
    let s = Settings::parse(&text).unwrap();
    let accounts = load_accounts(&s).accounts;
    let load = load_folders(&s, &accounts);
    assert_eq!(load.blocked_aliases, vec!["d"]);
    let by_alias = |a: &str| {
        load.folders
            .iter()
            .find(|f| f.definition.alias == a)
            .unwrap()
    };
    let acc = &accounts[0];
    let a = by_alias("a");
    assert_eq!(a.definition.target_path, "/remote");
    assert_eq!(a.definition.local_path, format!("{l}/"));
    // Old settings without journalPath get the default one.
    assert_eq!(
        a.definition.journal_path,
        a.definition.default_journal_path(acc)
    );
    assert!(a.definition.journal_path.starts_with(".sync_"));
    assert!(a.definition.ignore_hidden_files, "default true");
    let b = by_alias("b");
    assert_eq!(
        b.journal_migration,
        JournalMigration::FromAbsolutePath("/var/old/journal.db".into())
    );
    assert!(b.needs_save);
    assert!(b.definition.journal_path.starts_with(".sync_"));
    // No ._sync_ db exists: switch to the default name.
    assert!(by_alias("c").definition.journal_path.starts_with(".sync_"));
    let e = by_alias("e");
    assert_eq!(e.definition.virtual_files_mode, VfsMode::WithSuffix);
    assert!(e.definition.upgrade_vfs_mode);
    // Unknown mode: assumed off, like upstream.
    assert_eq!(by_alias("f").definition.virtual_files_mode, VfsMode::Off);
    // The Multifolders group is too new as a whole.
    assert!(load.folders.iter().all(|f| f.definition.alias != "g"));
}

#[test]
fn derived_journal_name_matches_make_db_name() {
    let dir = tempfile::tempdir().unwrap();
    let acc = AccountDefinition::new_webflow("0", "https://cloud.example.com", "alice");
    let d = FolderDefinition {
        local_path: format!("{}/", dir.path().display()),
        target_path: "/".into(),
        ..Default::default()
    };
    let expected =
        nc_journal::journal::make_db_name(dir.path(), "https://cloud.example.com", "/", "alice");
    assert_eq!(d.default_journal_path(&acc), expected);
}

#[test]
fn derived_save_remove_paused_and_aliases() {
    let mut s = Settings::parse(FIXTURE).unwrap();
    let def = FolderDefinition {
        alias: "New/One".into(),
        local_path: "/srv/new/".into(),
        journal_path: ".sync_aaaaaaaaaaaa.db".into(),
        target_path: "/New".into(),
        ..Default::default()
    };
    save_folder(&mut s, "0", FolderGroup::Folders, &def);
    let fg = folder_group("0", FolderGroup::Folders, "New__SLASH__One");
    assert_eq!(fg, "Accounts/0/Folders/New__SLASH__One");
    assert_eq!(FolderDefinition::load(&s, &fg, "New__SLASH__One"), def);
    assert_eq!(s.string(&format!("{fg}/version")), "2");
    assert_eq!(s.string(&format!("{fg}/virtualFilesMode")), "off");
    // Saving in another group removes it from the first.
    save_folder(&mut s, "0", FolderGroup::Multifolders, &def);
    assert!(s.child_keys(&fg).is_empty());
    assert!(save_paused(&mut s, "0", "New/One", true));
    assert!(s.bool_or("Accounts/0/Multifolders/New__SLASH__One/paused", false));
    assert!(!save_paused(&mut s, "0", "missing", true));
    // Only the paused line of an existing folder changes.
    assert!(save_paused(&mut s, "0", "1", true));
    assert_eq!(
        s.to_string()
            .replace("0\\Folders\\1\\paused=true", "0\\Folders\\1\\paused=false"),
        {
            let mut t = Settings::parse(FIXTURE).unwrap();
            save_folder(&mut t, "0", FolderGroup::Multifolders, &def);
            t.set_value("Accounts/0/Multifolders/New__SLASH__One/paused", true);
            t.to_string()
        }
    );
    remove_folder(&mut s, "0", "New__SLASH__One");
    assert!(s.child_groups("Accounts/0/Multifolders").is_empty());

    let aliases = all_aliases(&s);
    assert_eq!(aliases, ["1", "Docs/Work", "2"]);
    assert_eq!(allocate_alias("", &aliases, &[]), "3");
    assert_eq!(allocate_alias("1", &aliases, &[]), "3");
    assert_eq!(allocate_alias("Fresh", &aliases, &[]), "Fresh");
    assert_eq!(allocate_alias("", &[], &["1".to_owned()]), "2");
    assert_eq!(choose_group(false, false, true), FolderGroup::Folders);
    assert_eq!(choose_group(false, false, false), FolderGroup::Multifolders);
    assert_eq!(
        choose_group(true, true, true),
        FolderGroup::FoldersWithPlaceholders
    );
}

#[test]
fn derived_clean_path() {
    assert_eq!(clean_path("/a//b/./c/../d/"), "/a/b/d");
    assert_eq!(clean_path("/"), "/");
    assert_eq!(clean_path("a/../.."), "..");
    assert_eq!(clean_path(""), "");
}

// --- test/testfolderman.cpp ------------------------------------------------

/// `folderDefinition(path)` of testhelper.cpp, added to the folder list
/// like `FolderMan::addFolder`: `Folder` resolves its canonical path and
/// opens its journal, which creates the `.sync_*.db` file when the folder
/// exists.
fn add_folder(existing: &mut Vec<ExistingFolder>, path: &str, server: &ServerIdentity) {
    let def = FolderDefinition {
        local_path: FolderDefinition::prepare_local_path(path),
        target_path: FolderDefinition::prepare_target_path(path),
        alias: path.to_owned(),
        ..Default::default()
    };
    if Path::new(&def.local_path).is_dir() {
        std::fs::write(format!("{}.sync_0123456789ab.db", def.local_path), b"").unwrap();
    }
    existing.push(ExistingFolder {
        path: folder_canonical_path(&def.local_path),
        server: server.clone(),
    });
}

fn is_ok(r: Result<(), (PathValidityError, String)>) -> bool {
    r.is_ok()
}

#[test]
fn test_check_path_validity_for_new_folder() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    std::fs::create_dir_all(base.join("sub/ownCloud1/folder/f")).unwrap();
    std::fs::create_dir_all(base.join("ownCloud2")).unwrap();
    std::fs::create_dir_all(base.join("sub/free")).unwrap();
    std::fs::create_dir_all(base.join("free2/sub")).unwrap();
    std::fs::write(base.join("sub/file.txt"), b"hello").unwrap();
    let dir_path = std::fs::canonicalize(base)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let account = ServerIdentity {
        url: "http://example.de".into(),
        user: "testuser".into(),
    };
    let mut folders = Vec::new();
    add_folder(&mut folders, &format!("{dir_path}/sub/ownCloud1"), &account);
    add_folder(&mut folders, &format!("{dir_path}/ownCloud2"), &account);
    let check =
        |p: &str, s: Option<&ServerIdentity>| check_path_validity_for_new_folder(p, s, &folders);

    // those should be allowed
    assert!(is_ok(check(&format!("{dir_path}/sub/free"), None)));
    assert!(is_ok(check(&format!("{dir_path}/free2/"), None)));
    // Not an existing directory -> Ok
    assert!(is_ok(check(&format!("{dir_path}/sub/bliblablu"), None)));
    assert!(is_ok(check(
        &format!("{dir_path}/sub/free/bliblablu"),
        None
    )));

    // A file -> Error
    assert!(!is_ok(check(&format!("{dir_path}/sub/file.txt"), None)));

    // There are folders configured in those folders, url needs to be taken
    // into account: -> ERROR
    let url2 = account.clone();
    // The following both fail because they refer to the same account (user and url)
    assert!(!is_ok(check(
        &format!("{dir_path}/sub/ownCloud1"),
        Some(&url2)
    )));
    assert!(!is_ok(check(
        &format!("{dir_path}/ownCloud2/"),
        Some(&url2)
    )));

    // The following both fail because they are already sync folders even if
    // for another account
    let url3 = ServerIdentity {
        url: "http://anotherexample.org".into(),
        user: "dummy".into(),
    };
    for p in ["sub/ownCloud1", "ownCloud2"] {
        let path = format!("{dir_path}/{p}");
        assert_eq!(
            check(&path, Some(&url3)).unwrap_err().1,
            format!(
                "Please choose a different location. {path} is already being used as a sync folder."
            )
        );
    }

    assert!(!is_ok(check(&dir_path, None)));
    assert!(!is_ok(check(
        &format!("{dir_path}/sub/ownCloud1/folder"),
        None
    )));
    assert!(!is_ok(check(&format!("{dir_path}/sub/ownCloud1/f"), None)));

    // make a bunch of links
    let link = |target: &str, name: &str| {
        std::os::unix::fs::symlink(format!("{dir_path}/{target}"), format!("{dir_path}/{name}"))
            .unwrap();
    };
    link("sub/free", "link1");
    link("sub", "link2");
    link("sub/ownCloud1", "link3");
    link("sub/ownCloud1/folder", "link4");

    // Ok
    assert!(is_ok(check(&format!("{dir_path}/link1"), None)));
    assert!(is_ok(check(&format!("{dir_path}/link2/free"), None)));

    // Not Ok
    assert!(!is_ok(check(&format!("{dir_path}/link2"), None)));

    // link 3 points to an existing sync folder. To make it fail, the account
    // must be the same
    assert!(!is_ok(check(&format!("{dir_path}/link3"), Some(&url2))));
    // while with a different account, this is fine
    assert_eq!(
        check(&format!("{dir_path}/link3"), Some(&url3))
            .unwrap_err()
            .1,
        format!(
            "Please choose a different location. {dir_path}/link3 is already being used as a sync folder."
        )
    );

    assert!(!is_ok(check(&format!("{dir_path}/link4"), None)));
    assert!(!is_ok(check(&format!("{dir_path}/link3/folder"), None)));

    // test some non existing sub path (error)
    assert!(!is_ok(check(
        &format!("{dir_path}/sub/ownCloud1/some/sub/path"),
        None
    )));
    assert!(!is_ok(check(&format!("{dir_path}/ownCloud2/blublu"), None)));
    assert!(!is_ok(check(
        &format!("{dir_path}/sub/ownCloud1/folder/g/h"),
        None
    )));
    assert!(!is_ok(check(
        &format!("{dir_path}/link3/folder/neu_folder"),
        None
    )));

    // Subfolder of links
    assert!(is_ok(check(&format!("{dir_path}/link1/subfolder"), None)));
    assert!(is_ok(check(
        &format!("{dir_path}/link2/free/subfolder"),
        None
    )));

    // Should not have the rights (unless the tests run as root)
    if !rustix::process::geteuid().is_root() {
        assert!(!is_ok(check("/", None)));
        assert!(!is_ok(check("/usr/bin/somefolder", None)));
    }

    // Invalid paths
    assert!(!is_ok(check("", None)));

    // REMOVE ownCloud2 from the filesystem, but keep a folder sync'ed to it.
    std::fs::remove_dir_all(format!("{dir_path}/ownCloud2/")).unwrap();
    assert!(!is_ok(check(&format!("{dir_path}/ownCloud2/blublu"), None)));
    assert!(!is_ok(check(
        &format!("{dir_path}/ownCloud2/sub/subsub/sub"),
        None
    )));
}

#[test]
fn test_find_good_path_for_new_sync_folder() {
    // SETUP
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    for d in [
        "sub/ownCloud1/folder/f",
        "ownCloud",
        "ownCloud2",
        "ownCloud2/foo",
        "sub/free",
        "free2/sub",
    ] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let dir_path = std::fs::canonicalize(base)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let url = ServerIdentity {
        url: "http://example.de".into(),
        user: "testuser".into(),
    };
    let mut folders = Vec::new();
    add_folder(&mut folders, &format!("{dir_path}/sub/ownCloud/"), &url);
    add_folder(&mut folders, &format!("{dir_path}/ownCloud2/"), &url);
    let good = |p: &str, f: &[ExistingFolder]| {
        find_good_path_for_new_sync_folder(
            &format!("{dir_path}{p}"),
            Some(&url),
            f,
            GoodPathStrategy::AllowOnlyNewPath,
        )
    };

    // TEST
    assert_eq!(good("/oc", &folders), format!("{dir_path}/oc"));
    assert_eq!(good("/ownCloud", &folders), format!("{dir_path}/ownCloud3"));
    assert_eq!(
        good("/ownCloud2", &folders),
        format!("{dir_path}/ownCloud22")
    );
    assert_eq!(
        good("/ownCloud2/foo", &folders),
        format!("{dir_path}/ownCloud2/foo")
    );
    assert_eq!(
        good("/ownCloud2/bar", &folders),
        format!("{dir_path}/ownCloud2/bar")
    );
    assert_eq!(good("/sub", &folders), format!("{dir_path}/sub2"));

    // REMOVE ownCloud2 from the filesystem, but keep a folder sync'ed to it.
    // We should still not suggest this folder as a new folder.
    std::fs::remove_dir_all(format!("{dir_path}/ownCloud2/")).unwrap();
    assert_eq!(good("/ownCloud", &folders), format!("{dir_path}/ownCloud3"));
    assert_eq!(
        good("/ownCloud2", &folders),
        format!("{dir_path}/ownCloud22")
    );
}
