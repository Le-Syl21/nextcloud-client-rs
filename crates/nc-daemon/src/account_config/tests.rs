// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// Port of upstream test/testaccountmanager.cpp and test/testaccount.cpp
// (the account configuration parts; nextcloud/desktop v34.0.5), plus derived
// tests.

use super::*;

const FIXTURE: &str = include_str!("../../testdata/nextcloud.cfg");

#[test]
fn derived_load_fixture_accounts() {
    let s = Settings::parse(FIXTURE).unwrap();
    let load = load_accounts(&s);
    assert_eq!(load.result, RestoreResult::Success);
    assert_eq!(load.accounts.len(), 2);
    let a = &load.accounts[0];
    assert_eq!(a.id, "0");
    assert_eq!(a.url, "https://cloud.example.com");
    assert_eq!(a.auth_type, "webflow");
    assert_eq!(a.credentials_user(), "alice");
    assert_eq!(a.dav_user, "alice");
    assert_eq!(a.display_name, "Alice Liddell");
    assert_eq!(a.server_version, "29.0.4.1");
    assert_eq!(a.proxy.proxy_type, 2);
    assert_eq!(a.upload_limit_setting, LimitSetting::NoLimit);
    assert_eq!(a.url_string(), "https://cloud.example.com");
    let b = &load.accounts[1];
    assert_eq!(b.url, "https://work.example.org:8443/nextcloud");
    assert_eq!(b.credentials_user(), "bob@example.org");
    assert_eq!(
        b.proxy,
        ProxySettings {
            proxy_type: 3,
            host: "proxy.example.org".into(),
            port: 3128,
            needs_auth: true,
            user: "bob".into()
        }
    );
    assert_eq!(b.upload_limit_setting, LimitSetting::ManualLimit);
    assert_eq!(b.upload_limit, 100);
    assert_eq!(b.password_file, None);
}

#[test]
fn derived_save_load_round_trip_and_unknown_keys_survive() {
    let mut s = Settings::parse(FIXTURE).unwrap();
    let mut a = load_account(&s, "0").unwrap();
    a.display_name = "Alice L.".into();
    a.password_file = Some("/run/secret".into());
    save_account(&mut s, &a);
    let text = s.to_string();
    // Untouched typed values are kept byte for byte.
    assert!(text.contains(
        "0\\serverColor=@Variant(\\0\\0\\0\\x43\\x1\\xff\\xff\\0\\x82\\0\\xc9\\0\\xff\\0\\0)\n"
    ));
    assert!(text.contains("0\\displayName=Alice L.\n"));
    let again = load_account(&Settings::parse(&text).unwrap(), "0").unwrap();
    assert_eq!(again, a);

    // A brand new account.
    let mut fresh = Settings::new();
    let id = generate_free_account_id(&fresh, &[]);
    assert_eq!(id, "0");
    let mut n = AccountDefinition::new_webflow(&id, "https://n.example", "carol");
    n.server_version = "31.0.1".into();
    save_account(&mut fresh, &n);
    let loaded = load_accounts(&fresh);
    assert_eq!(loaded.accounts, vec![n]);
    assert_eq!(fresh.string("Accounts/version"), "13");
    assert_eq!(fresh.string("Accounts/0/webflow_user"), "carol");
    assert_eq!(generate_free_account_id(&fresh, &[]), "1");
    assert_eq!(generate_free_account_id(&fresh, &["1".to_owned()]), "2");
    remove_account(&mut fresh, "0");
    assert!(fresh.child_groups("Accounts").is_empty());
}

#[test]
fn derived_auth_type_fixups_and_http_migration() {
    let s = Settings::parse(
        "[Accounts]\n0\\url=https://a\n0\\authType=http\n0\\http_user=ann\n0\\user=ann\n\
         1\\url=https://b\n1\\authType=dummy\n1\\webflow_user=ben\n\
         2\\authType=webflow\n\
         3\\url=https://d\n3\\http_user=dan\nversion=13\n",
    )
    .unwrap();
    let load = load_accounts(&s);
    // Account 2 has no URL: skipped.
    let ids: Vec<_> = load.accounts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["0", "1", "3"]);
    let a = &load.accounts[0];
    assert_eq!(a.auth_type, "webflow");
    assert_eq!(
        a.credential_settings
            .get("webflow_user")
            .map(String::as_str),
        Some("ann")
    );
    assert_eq!(
        a.credential_settings.get("authType").map(String::as_str),
        Some("webflow")
    );
    assert_eq!(a.credentials_user(), "ann");
    assert_eq!(load.accounts[1].auth_type, "webflow");
    assert_eq!(load.accounts[1].credentials_user(), "ben");
    // Empty authType with http_user: http, then migrated to webflow.
    assert_eq!(load.accounts[2].auth_type, "webflow");
    assert_eq!(load.accounts[2].credentials_user(), "dan");
}

#[test]
fn derived_too_new_versions_are_skipped() {
    let s = Settings::parse(
        "[Accounts]\n0\\url=https://a\n0\\version=14\n1\\url=https://b\nversion=13\n",
    )
    .unwrap();
    let load = load_accounts(&s);
    assert_eq!(load.result, RestoreResult::SuccessWithSkipped);
    assert_eq!(load.blocked_ids, vec!["0"]);
    assert_eq!(load.accounts.len(), 1);
    assert_eq!(generate_free_account_id(&s, &load.blocked_ids), "2");

    let s = Settings::parse("[Accounts]\n0\\url=https://a\nversion=14\n").unwrap();
    let load = load_accounts(&s);
    assert_eq!(load.result, RestoreResult::SuccessWithSkipped);
    assert!(load.accounts.is_empty());

    assert_eq!(
        load_accounts(&Settings::new()).result,
        RestoreResult::NotFound
    );
}

#[test]
fn derived_legacy_global_limit_takes_the_client_wide_values() {
    let s = Settings::parse(
        "[Accounts]\n0\\url=https://a\n0\\networkUploadLimitSetting=-2\n0\\networkDownloadLimitSetting=-1\n\
         version=13\n\n[BWLimit]\nuseUploadLimit=1\nuploadLimit=42\n",
    )
    .unwrap();
    let a = &load_accounts(&s).accounts[0];
    assert_eq!(a.upload_limit_setting, LimitSetting::ManualLimit);
    assert_eq!(a.upload_limit, 42);
    assert_eq!(a.download_limit_setting, LimitSetting::NoLimit);
}

#[test]
fn derived_find_account() {
    let s = Settings::parse(FIXTURE).unwrap();
    let accounts = load_accounts(&s).accounts;
    assert_eq!(
        find_account(&accounts, "https://cloud.example.com/", "alice").map(|a| a.id.as_str()),
        Some("0")
    );
    assert!(find_account(&accounts, "https://cloud.example.com", "bob").is_none());
}

// --- test/testaccountmanager.cpp -------------------------------------------

#[test]
fn restores_public_share_link_with_basic_credentials() {
    let account_id = "public-share";
    let share_url = "https://cloud.example/s/share-token";
    for auth_type in ["http", "webflow"] {
        let user_key = if auth_type == "http" {
            "http_user"
        } else {
            "webflow_user"
        };
        let mut s = Settings::new();
        s.set_value("Accounts/version", 13);
        s.set_value(&format!("Accounts/{account_id}/version"), 13);
        s.set_value(&format!("Accounts/{account_id}/url"), share_url);
        s.set_value(&format!("Accounts/{account_id}/authType"), auth_type);
        s.set_value(&format!("Accounts/{account_id}/{user_key}"), "share-token");
        s.set_value(&format!("Accounts/{account_id}/dav_user"), "share-token");

        let load = load_accounts(&s);
        assert_eq!(load.result, RestoreResult::Success);
        assert_eq!(load.accounts.len(), 1);
        let a = &load.accounts[0];
        assert_eq!(a.auth_type, "http", "{auth_type}");
        assert!(a.public_share_link.is_some());
        assert_eq!(a.credentials_user(), "share-token");
        assert_eq!(a.url_string(), "https://share-token@cloud.example");
    }
}

// --- test/testaccount.cpp --------------------------------------------------

#[test]
fn test_account_is_public_share_link() {
    let rows: &[(&str, &str, bool, &str)] = &[
        ("plain URL", "https://example.com", false, ""),
        (
            "plain URL, trailing slash",
            "https://example.com/",
            false,
            "",
        ),
        (
            "share link",
            "https://example.com/s/rPZLaTKfWct37Nd",
            true,
            "rPZLaTKfWct37Nd",
        ),
        (
            "share link, trailing slash",
            "https://example.com/s/rPZLaTKfWct37Nd/",
            false,
            "",
        ),
        (
            "subpath containing /s/ (looks like share link)",
            "https://example.com/s/nextcloud",
            true,
            "nextcloud",
        ),
        (
            "subpath containing /s/, trailing slash",
            "https://example.com/s/nextcloud/",
            false,
            "",
        ),
        (
            "subpath containing /s/, share link",
            "https://example.com/s/nextcloud/s/rPZLaTKfWct37Nd",
            true,
            "rPZLaTKfWct37Nd",
        ),
        (
            "subpath containing /s/, share link, trailing slash",
            "https://example.com/s/nextcloud/s/rPZLaTKfWct37Nd/",
            false,
            "",
        ),
    ];
    for (name, url, expected, dav_user) in rows {
        let parts = public_share_link_parts(url);
        assert_eq!(parts.is_some(), *expected, "{name}");
        // Account::setUrl sets the DAV user to the token.
        assert_eq!(
            parts.map(|(_, t)| t).unwrap_or_default(),
            *dav_user,
            "{name}"
        );
    }
}

#[test]
fn test_account_set_limit_settings_global_network_limit_fallback() {
    let mut account = AccountDefinition::default();
    let set = |a: &mut AccountDefinition, s| {
        a.set_download_limit_setting(s);
        a.set_upload_limit_setting(s);
    };
    let verify = |a: &AccountDefinition, s| {
        assert_eq!(a.download_limit_setting, s);
        assert_eq!(a.upload_limit_setting, s);
    };
    // the default setting should be NoLimit
    verify(&account, LimitSetting::NoLimit);
    // changing it to ManualLimit should succeed
    set(&mut account, LimitSetting::ManualLimit);
    verify(&account, LimitSetting::ManualLimit);
    // changing it to AutoLimit should fall back to NoLimit
    set(&mut account, LimitSetting::AutoLimit);
    verify(&account, LimitSetting::NoLimit);
    // changing it to LegacyGlobalLimit (-2) should fall back to NoLimit
    set(&mut account, LimitSetting::LegacyGlobalLimit);
    verify(&account, LimitSetting::NoLimit);
}
