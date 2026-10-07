// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

const FIXTURE: &str = include_str!("../../testdata/nextcloud.cfg");

fn official() -> Settings {
    Settings::parse(FIXTURE).unwrap()
}

fn group_raw(s: &Settings, group: &str) -> Vec<(String, Option<String>)> {
    s.all_keys(group)
        .into_iter()
        .map(|k| {
            let v = s.raw_value(&join_key(group, &k));
            (k, v)
        })
        .collect()
}

#[test]
fn derived_take_over_and_hand_back_round_trip() {
    let mut off = official();
    let mut ours = Settings::new();
    let original_folder = group_raw(&off, "Accounts/0/Folders/Docs__SLASH__Work");
    let original_account: Vec<_> = group_raw(&off, "Accounts/0")
        .into_iter()
        .filter(|(k, _)| !k.starts_with("Folders/"))
        .collect();

    let out = take_over(
        &mut off,
        &mut ours,
        "Docs/Work",
        Path::new("/o/nextcloud.cfg"),
    )
    .unwrap();
    assert_eq!(out.alias, "Docs/Work");
    assert_eq!(out.account_id, "0");
    assert!(out.created_account);
    assert_eq!(out.official_account.credentials_user(), "alice");
    assert_eq!(out.folder.local_path, "/home/alice/My Docs, 2024/");

    // Ours: the account (without its folders) and the folder, verbatim.
    assert_eq!(
        group_raw(&ours, "Accounts/0/Folders/Docs__SLASH__Work"),
        original_folder
    );
    assert_eq!(
        group_raw(&ours, "Accounts/0").len(),
        original_account.len() + original_folder.len()
    );
    for (k, v) in &original_account {
        assert_eq!(&ours.raw_value(&join_key("Accounts/0", k)), v, "{k}");
    }
    assert_eq!(ours.string("Accounts/version"), "13");
    assert_eq!(
        group_raw(&ours, "ncsyncdTakeover/Docs__SLASH__Work/folder"),
        original_folder
    );
    assert_eq!(
        ours.string("ncsyncdTakeover/Docs__SLASH__Work/source"),
        "/o/nextcloud.cfg"
    );
    assert_eq!(taken_over_aliases(&ours), vec!["Docs/Work"]);
    let our_folders = load_folders(&ours, &load_accounts(&ours).accounts).folders;
    assert_eq!(our_folders.len(), 1);
    assert_eq!(
        our_folders[0].definition.journal_path,
        ".sync_0a1b2c3d4e5f.db"
    );
    assert!(our_folders[0].definition.paused);

    // Official: the folder is gone, the rest is untouched.
    assert!(
        off.all_keys("Accounts/0/Folders/Docs__SLASH__Work")
            .is_empty()
    );
    assert_eq!(off.child_groups("Accounts/0/Folders"), vec!["1"]);
    assert_eq!(
        off.to_string(),
        FIXTURE
            .lines()
            .filter(|l| !l.starts_with("0\\Folders\\Docs__SLASH__Work\\"))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
    );

    // Taking it over again: it is not in the official file any more.
    assert!(matches!(
        take_over(&mut off, &mut ours, "Docs/Work", Path::new("/o")),
        Err(TakeoverError::NotFound(_))
    ));

    // Hand-back restores the same keys and values and empties ours.
    let back = hand_back(&mut ours, &mut off, "Docs/Work").unwrap();
    assert_eq!(back.official_alias, "Docs/Work");
    assert_eq!(back.official_account_id, "0");
    assert_eq!(
        back.removed_account.as_ref().map(|a| a.id.as_str()),
        Some("0")
    );
    assert_eq!(
        group_raw(&off, "Accounts/0/Folders/Docs__SLASH__Work"),
        original_folder
    );
    assert!(ours.all_keys("Accounts/0").is_empty());
    assert!(ours.child_groups("ncsyncdTakeover").is_empty());
    assert!(matches!(
        hand_back(&mut ours, &mut off, "Docs/Work"),
        Err(TakeoverError::NoBackup(_))
    ));
}

#[test]
fn derived_take_over_by_path_reuses_our_account() {
    let mut off = official();
    let mut ours = Settings::new();
    // Ours already has bob's account under another id.
    let mut bob = load_account(&off, "1").unwrap();
    bob.id = "7".into();
    crate::account_config::save_account(&mut ours, &bob);

    let out = take_over(&mut off, &mut ours, "/home/alice/Médias", Path::new("/o")).unwrap();
    assert_eq!(out.account_id, "7");
    assert!(!out.created_account);
    assert_eq!(out.alias, "2");
    assert_eq!(
        ours.string("Accounts/7/Multifolders/2/localPath"),
        "/home/alice/Médias/"
    );
    assert!(off.child_groups("Accounts/1/Multifolders").is_empty());

    let back = hand_back(&mut ours, &mut off, "2").unwrap();
    assert!(
        back.removed_account.is_none(),
        "the account was ours before"
    );
    assert!(ours.contains("Accounts/7/url"));
    assert_eq!(
        off.string("Accounts/1/Multifolders/2/localPath"),
        "/home/alice/Médias/"
    );
}

#[test]
fn derived_take_over_refusals() {
    let mut off = official();
    let mut ours = Settings::new();
    // Alias 1 exists in both accounts of the fixture.
    assert!(matches!(
        take_over(&mut off, &mut ours, "1", Path::new("/o")),
        Err(TakeoverError::Ambiguous(..))
    ));
    // Virtual files.
    assert!(matches!(
        take_over(&mut off, &mut ours, "/home/alice/Work VFS/", Path::new("/o")),
        Err(TakeoverError::Unsupported(m)) if m.contains("virtual files")
    ));
    assert!(matches!(
        take_over(&mut off, &mut ours, "/nowhere", Path::new("/o")),
        Err(TakeoverError::NotFound(_))
    ));
    // Ours already syncs that path.
    let acc = load_account(&off, "0").unwrap();
    crate::account_config::save_account(&mut ours, &acc);
    let def = FolderDefinition {
        alias: "mine".into(),
        local_path: "/home/alice/Nextcloud/".into(),
        journal_path: ".sync_6f3d2b8a1c04.db".into(),
        target_path: "/".into(),
        ..Default::default()
    };
    crate::folder_definition::save_folder(&mut ours, "0", FolderGroup::Folders, &def);
    let before = off.to_string();
    assert!(matches!(
        take_over(
            &mut off,
            &mut ours,
            "/home/alice/Nextcloud",
            Path::new("/o")
        ),
        Err(TakeoverError::AlreadyOurs(_))
    ));
    assert_eq!(off.to_string(), before, "nothing changed");
}

#[test]
fn derived_hand_back_refusals_and_alias_clash() {
    let mut off = official();
    let mut ours = Settings::new();
    take_over(&mut off, &mut ours, "Docs/Work", Path::new("/o")).unwrap();
    // The official client got another folder with that alias meanwhile.
    off.set_value(
        "Accounts/0/Folders/Docs__SLASH__Work/localPath",
        "/elsewhere/",
    );
    let back = hand_back(&mut ours.clone(), &mut off.clone(), "Docs/Work").unwrap();
    assert_eq!(back.official_alias, "3");
    // ... or a folder for the same path.
    let mut off2 = off.clone();
    off2.set_value(
        "Accounts/0/Folders/9/localPath",
        "/home/alice/My Docs, 2024/",
    );
    assert!(matches!(
        hand_back(&mut ours.clone(), &mut off2, "Docs/Work"),
        Err(TakeoverError::OfficialHasFolder(_))
    ));
    // The official account was removed.
    let mut off3 = off.clone();
    off3.remove("Accounts/0");
    assert!(matches!(
        hand_back(&mut ours, &mut off3, "Docs/Work"),
        Err(TakeoverError::OfficialAccountGone(id)) if id == "0"
    ));
}

#[test]
fn derived_files_layer() {
    let dir = tempfile::tempdir().unwrap();
    let off_path = dir.path().join("Nextcloud/nextcloud.cfg");
    let our_path = dir.path().join("ncsyncd/ncsyncd.cfg");
    std::fs::create_dir_all(off_path.parent().unwrap()).unwrap();
    std::fs::write(&off_path, FIXTURE).unwrap();
    let no_check: ProcessCheck = |_| Ok(());
    let out = take_over_files_checked(&off_path, &our_path, "Docs/Work", no_check).unwrap();
    assert_eq!(out.alias, "Docs/Work");
    let ours = Settings::load(&our_path).unwrap();
    assert_eq!(handback_source(&ours, "Docs/Work"), Some(off_path.clone()));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("Nextcloud/nextcloud.cfg.ncsyncd-bak")).unwrap(),
        FIXTURE
    );
    assert!(!dir.path().join("Nextcloud/nextcloud.cfg.lock").exists());
    hand_back_files_checked(&our_path, &off_path, "Docs/Work", no_check).unwrap();
    let off = Settings::load(&off_path).unwrap();
    assert_eq!(
        off.string("Accounts/0/Folders/Docs__SLASH__Work/targetPath"),
        "/Documents/Work"
    );
    // A running client is refused before anything is touched.
    let refuse: ProcessCheck = |_| {
        Err(TakeoverError::OfficialClientRunning(vec![RunningProcess {
            pid: 42,
            name: "nextcloud".into(),
        }]))
    };
    let err = take_over_files_checked(&off_path, &our_path, "Docs/Work", refuse).unwrap_err();
    assert!(err.to_string().contains("pid 42"));
}

#[test]
fn derived_official_client_process_matching() {
    assert!(is_official_client("nextcloud", None, ""));
    assert!(is_official_client("x", Some("/usr/bin/nextcloud"), ""));
    assert!(is_official_client(
        "AppRun",
        None,
        "/home/a/Nextcloud-3.16.4-x86_64.AppImage"
    ));
    assert!(is_official_client(
        "x",
        Some("/tmp/.mount_NextclAbc/usr/bin/nextcloud"),
        ""
    ));
    assert!(!is_official_client(
        "nextcloudcmd",
        Some("/usr/bin/nextcloudcmd"),
        "nextcloudcmd"
    ));
    assert!(!is_official_client("ncsync", None, "ncsync"));
    // The scan itself works (whatever runs on this machine).
    let _ = official_client_processes(Some(0));
}
