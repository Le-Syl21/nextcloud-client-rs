// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

const FIXTURE: &str = include_str!("../../testdata/nextcloud.cfg");

#[test]
fn rust_only_fixture_reads_like_qsettings() {
    let s = Settings::parse(FIXTURE).unwrap();
    // Root keys live in [General].
    assert_eq!(s.string("clientVersion"), "3.16.4git (build 20250331)");
    assert!(s.bool_or("launchOnSystemStartup", false));
    assert_eq!(s.i64_or("newBigFolderSizeLimit", 0), 500);
    assert_eq!(s.string("overrideServerUrl"), "");
    assert!(s.contains("overrideServerUrl"));
    // Group keys with `\` subkeys.
    assert_eq!(s.child_groups("Accounts"), vec!["0", "1"]);
    assert_eq!(s.child_keys("Accounts"), vec!["version"]);
    assert_eq!(s.i64_or("Accounts/version", 0), 13);
    assert_eq!(s.string("Accounts/0/url"), "https://cloud.example.com");
    assert_eq!(
        s.child_groups("Accounts/0/Folders"),
        vec!["1", "Docs__SLASH__Work"]
    );
    assert_eq!(
        s.string("Accounts/0/Folders/Docs__SLASH__Work/localPath"),
        "/home/alice/My Docs, 2024/"
    );
    assert!(s.bool_or("Accounts/0/Folders/Docs__SLASH__Work/paused", false));
    assert_eq!(
        s.string("Accounts/1/Multifolders/2/localPath"),
        "/home/alice/Médias/"
    );
    assert_eq!(s.string("BWLimit/downloadLimit"), "80");
    assert_eq!(s.i64_or("Nextcloud/remotePollInterval", 0), 60000);
    // Typed values.
    assert_eq!(
        s.value("Accounts/0/encryptionCertificateSha256Fingerprint"),
        Some(Value::ByteArray(Vec::new()))
    );
    assert_eq!(
        s.value("Accounts/0/serverColor"),
        Some(Value::Variant(vec![
            0, 0, 0, 0x43, 1, 0xff, 0xff, 0, 0x82, 0, 0xc9, 0, 0xff, 0, 0
        ]))
    );
    assert_eq!(s.value("nope"), None);
    let keys = s.child_keys("Accounts/0");
    assert!(keys.contains(&"webflow_user".to_owned()));
    assert!(!keys.contains(&"Folders".to_owned()));
}

#[test]
fn rust_only_unmodified_round_trip_is_identical() {
    let s = Settings::parse(FIXTURE).unwrap();
    assert_eq!(s.to_string(), FIXTURE);
}

#[test]
fn rust_only_written_from_scratch_is_byte_identical_to_qsettings() {
    // A configuration written by us, key by key (in the order QSettings
    // writes them: sections and keys sorted), is the file QSettings wrote.
    let order = Settings::parse(FIXTURE).unwrap().entries();
    let mut s = Settings::new();
    for e in &order {
        s.set_value(&e.path, unescape_value(&e.raw));
    }
    assert_eq!(s.to_string(), FIXTURE);
    // Saved to a file, byte for byte.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nextcloud.cfg");
    s.set_file_path(&path);
    s.save().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), FIXTURE.as_bytes());
}

#[test]
fn rust_only_rewriting_every_value_keeps_the_file() {
    // Re-encoding every decoded value gives the text QSettings wrote.
    let mut s = Settings::parse(FIXTURE).unwrap();
    for key in s.all_keys("") {
        let v = s.value(&key).unwrap();
        s.set_value(&key, v);
    }
    assert_eq!(s.to_string(), FIXTURE);
}

#[test]
fn rust_only_new_keys_go_to_their_section() {
    let mut s = Settings::parse(FIXTURE).unwrap();
    s.set_value("Accounts/2/url", "https://new.example.net");
    s.set_value("Accounts/2/Folders/1/paused", true);
    s.set_value("timeout", 120);
    s.set_value("General/odd", "x");
    s.set_value("Nextcloud/forceSyncInterval", 7_200_000_i64);
    let text = s.to_string();
    assert!(text.contains("2\\url=https://new.example.net\n"));
    assert!(text.contains("2\\Folders\\1\\paused=true\n"));
    // A new key goes right after the section's last key, before the blank
    // line that separates sections, like QSettings writes it.
    assert!(text.contains("useNewBigFolderSizeLimit=true\ntimeout=120\n\n[Accounts]"));
    assert!(text.contains("[%General]\nodd=x\n"));
    let s2 = Settings::parse(&text).unwrap();
    assert_eq!(s2.string("Accounts/2/url"), "https://new.example.net");
    assert!(s2.bool_or("Accounts/2/Folders/1/paused", false));
    assert_eq!(s2.i64_or("timeout", 0), 120);
    assert_eq!(s2.string("General/odd"), "x");
    assert_eq!(s2.child_groups("Accounts"), vec!["0", "1", "2"]);
    assert_eq!(
        s2.child_groups(""),
        vec!["Accounts", "BWLimit", "General", "Nextcloud", "Proxy"]
    );
}

#[test]
fn rust_only_remove_group_and_empty_section() {
    let mut s = Settings::parse(FIXTURE).unwrap();
    s.remove("Accounts/0/Folders/Docs__SLASH__Work");
    assert_eq!(s.child_groups("Accounts/0/Folders"), vec!["1"]);
    assert!(s.contains("Accounts/0/Folders/1/localPath"));
    s.remove("Proxy");
    assert!(!s.to_string().contains("[Proxy]"));
    s.remove("Accounts/1");
    assert_eq!(s.child_groups("Accounts"), vec!["0"]);
    s.remove("");
    assert_eq!(s.to_string().trim(), "");
}

#[test]
fn rust_only_key_escaping() {
    assert_eq!(escape_key("a/b c"), "a\\b%20c");
    assert_eq!(escape_key("x_y-z.1"), "x_y-z.1");
    assert_eq!(escape_key("é"), "%E9");
    assert_eq!(escape_key("€"), "%U20AC");
    assert_eq!(escape_key("%"), "%25");
    for k in ["a b", "é€", "50%", "x=y", "Ünïcödé key", "🎉"] {
        assert_eq!(unescape_key(&escape_key(k)), k, "{k}");
    }
    assert_eq!(unescape_key("a\\b"), "a/b");
    assert_eq!(unescape_key("a/b"), "a/b");
    assert_eq!(unescape_key("100%"), "100%");
    assert_eq!(unescape_key("%zz"), "%zz");
    assert_eq!(unescape_key("%U00e9"), "é");
    assert_eq!(normalize_key("//a///b/"), "a/b");

    let mut s = Settings::new();
    s.set_value("My Group/key with space", "v");
    assert_eq!(s.to_string(), "[My%20Group]\nkey%20with%20space=v\n");
    assert_eq!(s.string("My Group/key with space"), "v");
}

#[test]
fn rust_only_value_escaping() {
    let cases: &[(&str, &str)] = &[
        ("plain", "plain"),
        ("", ""),
        ("a,b", "\"a,b\""),
        ("a;b", "\"a;b\""),
        ("k=v", "\"k=v\""),
        (" lead", "\" lead\""),
        ("trail ", "\"trail \""),
        ("back\\slash", "back\\\\slash"),
        ("quo\"te", "quo\\\"te"),
        ("line\nnext\ttab", "line\\nnext\\ttab"),
        ("\u{1}2", "\\x1\\x32"),
        ("@at", "@@at"),
        ("Médias ✓", "Médias ✓"),
    ];
    for (input, written) in cases {
        let v = Value::String((*input).to_owned());
        assert_eq!(escape_value(&v), *written, "writing {input:?}");
        assert_eq!(unescape_value(written), v, "reading {written:?}");
    }
    // Lists.
    let list = Value::StringList(vec!["a".into(), "b c".into(), "d,e".into()]);
    assert_eq!(escape_value(&list), "a, b c, \"d,e\"");
    assert_eq!(unescape_value("a, b c, \"d,e\""), list);
    assert_eq!(
        unescape_value("a,b"),
        Value::StringList(vec!["a".into(), "b".into()])
    );
    assert_eq!(escape_value(&Value::StringList(Vec::new())), "@Invalid()");
    assert_eq!(unescape_value("@Invalid()"), Value::Invalid);
    assert_eq!(escape_value(&Value::StringList(vec!["one".into()])), "one");
    // Typed values.
    assert_eq!(
        escape_value(&Value::ByteArray(b"a,\xe9\x01".to_vec())),
        "\"@ByteArray(a,\\xe9\\x1)\""
    );
    assert_eq!(
        unescape_value("\"@ByteArray(a,\\xe9\\x1)\""),
        Value::ByteArray(b"a,\xe9\x01".to_vec())
    );
    assert_eq!(unescape_value("@String(x)"), Value::String("x".into()));
    // Spaces around unquoted values are not part of them.
    assert_eq!(unescape_value("  v  "), Value::String("v".into()));
    // Octal escape and skipped unknown escape.
    assert_eq!(unescape_value("\\101\\q"), Value::String("A".into()));
}

#[test]
fn rust_only_conversions() {
    for (s, b) in [
        ("true", true),
        ("false", false),
        ("0", false),
        ("1", true),
        ("", false),
        ("FALSE", false),
        ("yes", true),
    ] {
        assert_eq!(Value::String(s.into()).to_bool(), b, "{s}");
    }
    assert_eq!(Value::String(" 42 ".into()).to_i64(), Some(42));
    assert_eq!(Value::String("x".into()).to_i64(), None);
    assert_eq!(Value::String("a".into()).to_string_list(), vec!["a"]);
    assert!(Value::Invalid.to_string_list().is_empty());
}

#[test]
fn rust_only_general_section_spellings() {
    let s = Settings::parse("[general]\na=1\n[%General]\nb=2\n[Accounts\\0]\nurl=u\n").unwrap();
    assert_eq!(s.string("a"), "1");
    assert_eq!(s.string("General/b"), "2");
    assert_eq!(s.string("Accounts/0/url"), "u");
    assert_eq!(s.child_groups("Accounts"), vec!["0"]);
}

#[test]
fn rust_only_save_load_modify_and_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/test.cfg");
    let mut s = Settings::load(&path).unwrap();
    assert!(s.all_keys("").is_empty());
    s.set_value("Accounts/0/url", "https://x");
    s.save().unwrap();
    assert!(!dir.path().join("sub/test.cfg.lock").exists());
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let ((), after) = Settings::modify(&path, |s| s.set_value("Accounts/0/dav_user", "u")).unwrap();
    assert_eq!(after.string("Accounts/0/url"), "https://x");
    let again = Settings::load(&path).unwrap();
    assert_eq!(again.string("Accounts/0/dav_user"), "u");

    // A lock held by a live process blocks; a stale one is taken over.
    let lock_path = dir.path().join("sub/test.cfg.lock");
    std::fs::write(&lock_path, "999999999\nnextcloud\nhost\n").unwrap();
    s.save().unwrap();
    let held = LockFile::acquire(&path).unwrap();
    assert!(lock_path.exists());
    drop(held);
    assert!(!lock_path.exists());
}
