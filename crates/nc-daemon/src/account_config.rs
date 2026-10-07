// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2015 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the load/save half of upstream `src/gui/accountmanager.cpp`, with
// the account URL rule of `Account::setUrl` and the limit setters of
// `src/libsync/account.cpp` (nextcloud/desktop v34.0.5).

//! Account definitions in `[Accounts]`: data only (the daemon builds the
//! `nc_dav::Account` from them).
//!
//! What is loaded follows `AccountManager::loadAccountHelper`: the URL, the
//! auth type with its fix-ups and the `http` → `webflow` migration, the
//! credential settings (`user` and every `<authType>_*` key), the DAV user,
//! display name, server version, per-account proxy and bandwidth limits.
//! Keys this module does not know (`serverColor`, `CaCertificates`, file
//! provider ids, ...) are never touched, so they survive a rewrite.

use std::collections::BTreeMap;

use crate::config_file::ConfigFile;
use crate::settings::{Settings, join_key};

/// `[Accounts]`.
pub const ACCOUNTS_GROUP: &str = "Accounts";
/// `maxAccountsVersion`: the highest `[Accounts] version` this client reads.
pub const MAX_ACCOUNTS_VERSION: i64 = 13;
/// `maxAccountVersion`: the highest per-account `version` this client reads.
pub const MAX_ACCOUNT_VERSION: i64 = 13;

const URL: &str = "url";
const AUTH_TYPE: &str = "authType";
const USER: &str = "user";
const DISPLAY_NAME: &str = "displayName";
const HTTP_USER: &str = "http_user";
const DAV_USER: &str = "dav_user";
const WEBFLOW_USER: &str = "webflow_user";
const SHIBBOLETH_USER: &str = "shibboleth_shib_user";
const VERSION: &str = "version";
const SERVER_VERSION: &str = "serverVersion";
const NETWORK_PROXY_TYPE: &str = "networkProxyType";
const NETWORK_PROXY_HOST_NAME: &str = "networkProxyHostName";
const NETWORK_PROXY_PORT: &str = "networkProxyPort";
const NETWORK_PROXY_NEEDS_AUTH: &str = "networkProxyNeedsAuth";
const NETWORK_PROXY_USER: &str = "networkProxyUser";
const NETWORK_UPLOAD_LIMIT_SETTING: &str = "networkUploadLimitSetting";
const NETWORK_DOWNLOAD_LIMIT_SETTING: &str = "networkDownloadLimitSetting";
const NETWORK_UPLOAD_LIMIT: &str = "networkUploadLimit";
const NETWORK_DOWNLOAD_LIMIT: &str = "networkDownloadLimit";

/// Our extension: a file holding the app password (first line), for
/// accounts whose secret is not in the keyring or systemd credentials. The
/// official client ignores the key.
pub const PASSWORD_FILE_KEY: &str = "ncsyncd_passwordFile";

const DUMMY_AUTH_TYPE: &str = "dummy";
/// `http` (HTTP basic with a password or app password).
pub const HTTP_AUTH_TYPE: &str = "http";
/// `webflow` (Login Flow v2 app password, also basic auth on the wire).
pub const WEBFLOW_AUTH_TYPE: &str = "webflow";
const SHIBBOLETH_AUTH_TYPE: &str = "shibboleth";
const HTTP_AUTH_PREFIX: &str = "http_";
const WEBFLOW_AUTH_PREFIX: &str = "webflow_";

/// `Account::AccountNetworkTransferLimitSetting`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LimitSetting {
    /// -2: until 3.17.0 "use the global setting"; now migrated.
    LegacyGlobalLimit,
    /// -1: the removed automatic limit.
    AutoLimit,
    /// 0.
    #[default]
    NoLimit,
    /// 1.
    ManualLimit,
}

impl LimitSetting {
    pub fn from_int(v: i64) -> Self {
        match v {
            -2 => Self::LegacyGlobalLimit,
            -1 => Self::AutoLimit,
            1 => Self::ManualLimit,
            _ => Self::NoLimit,
        }
    }

    pub fn to_int(self) -> i64 {
        match self {
            Self::LegacyGlobalLimit => -2,
            Self::AutoLimit => -1,
            Self::NoLimit => 0,
            Self::ManualLimit => 1,
        }
    }

    /// `Account::setUploadLimitSetting` / `setDownloadLimitSetting`: the
    /// legacy global limit and the automatic limit fall back to no limit.
    pub fn sanitized(self) -> Self {
        match self {
            Self::LegacyGlobalLimit | Self::AutoLimit => Self::NoLimit,
            s => s,
        }
    }
}

/// Per-account proxy (`QNetworkProxy::ProxyType` numbering: 0 default/system,
/// 1 SOCKS5, 2 none, 3 HTTP, 4 HTTP caching).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxySettings {
    pub proxy_type: i64,
    pub host: String,
    pub port: i64,
    pub needs_auth: bool,
    pub user: String,
}

/// One account of `[Accounts]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountDefinition {
    /// The account id (the group name: `0`, `1`, ...).
    pub id: String,
    /// The account URL as `Account::url()` renders it (for a public share
    /// link, the server part only).
    pub url: String,
    /// For a public share link, the link as configured.
    pub public_share_link: Option<String>,
    /// `webflow` or `http` (after upstream's fix-ups and migration).
    pub auth_type: String,
    pub dav_user: String,
    pub display_name: String,
    pub server_version: String,
    /// `Account::_settingsMap`: `user` and the `<authType>_*` keys.
    pub credential_settings: BTreeMap<String, String>,
    pub proxy: ProxySettings,
    pub upload_limit_setting: LimitSetting,
    pub upload_limit: i64,
    pub download_limit_setting: LimitSetting,
    pub download_limit: i64,
    /// [`PASSWORD_FILE_KEY`], our extension.
    pub password_file: Option<String>,
}

/// `Account::setUrl`'s public share link detection: a URL ending in
/// `/s/<token>` (no trailing slash). Returns the server part
/// (`scheme://host`) and the token.
pub fn public_share_link_parts(url: &str) -> Option<(String, String)> {
    // ((https|http)://[^/]*).*/s/([^/]*)$
    let scheme_len = if url.starts_with("https://") {
        8
    } else if url.starts_with("http://") {
        7
    } else {
        return None;
    };
    let host_end = url[scheme_len..]
        .find('/')
        .map_or(url.len(), |i| scheme_len + i);
    let base = &url[..host_end];
    let rest = &url[host_end..];
    let idx = rest.rfind("/s/")?;
    let token = &rest[idx + 3..];
    if token.contains('/') {
        return None;
    }
    Some((base.to_owned(), token.to_owned()))
}

impl AccountDefinition {
    /// `credentialSetting(key)`: `<authType>_<key>`, else `<key>`.
    pub fn credential_setting(&self, key: &str) -> Option<&str> {
        self.credential_settings
            .get(&format!("{}_{key}", self.auth_type))
            .or_else(|| self.credential_settings.get(key))
            .map(String::as_str)
    }

    /// `credentials()->user()`: the login name used for basic auth.
    pub fn credentials_user(&self) -> String {
        self.credential_setting(USER).unwrap_or_default().to_owned()
    }

    /// `Account::davUser()`: the explicit DAV user, else the credentials'.
    pub fn effective_dav_user(&self) -> String {
        if self.dav_user.is_empty() {
            self.credentials_user()
        } else {
            self.dav_user.clone()
        }
    }

    /// `account->url().toString()` as the journal name and keychain keys
    /// use it. For a public share link, `Account::setUrl` puts the token in
    /// the URL's user name.
    pub fn url_string(&self) -> String {
        if let Some((_, token)) = self
            .public_share_link
            .as_deref()
            .and_then(public_share_link_parts)
            && let Some((scheme, rest)) = self.url.split_once("://")
        {
            return format!("{scheme}://{token}@{rest}");
        }
        self.url.clone()
    }

    /// A new webflow account (what Login Flow v2 or an app password gives).
    pub fn new_webflow(id: &str, url: &str, login_name: &str) -> Self {
        let mut credential_settings = BTreeMap::new();
        credential_settings.insert(WEBFLOW_USER.to_owned(), login_name.to_owned());
        Self {
            id: id.to_owned(),
            url: url.to_owned(),
            auth_type: WEBFLOW_AUTH_TYPE.to_owned(),
            dav_user: login_name.to_owned(),
            credential_settings,
            // Proxy type 0 (DefaultProxy): the system settings.
            ..Self::default()
        }
    }

    /// `Account::setUploadLimitSetting`.
    pub fn set_upload_limit_setting(&mut self, setting: LimitSetting) {
        self.upload_limit_setting = setting.sanitized();
    }

    /// `Account::setDownloadLimitSetting`.
    pub fn set_download_limit_setting(&mut self, setting: LimitSetting) {
        self.download_limit_setting = setting.sanitized();
    }
}

fn account_group(id: &str) -> String {
    join_key(ACCOUNTS_GROUP, id)
}

/// `AccountManager::backwardMigrationSettingsKeys`: groups too new for this
/// client. Returns (`deleteKeys`, `ignoreKeys`) as full group paths
/// (`Accounts`, `Accounts/<id>`).
pub fn backward_migration_settings_keys(settings: &Settings) -> (Vec<String>, Vec<String>) {
    let mut delete_keys = Vec::new();
    let mut ignore_keys = Vec::new();
    let accounts_version = settings.i64_or(&join_key(ACCOUNTS_GROUP, VERSION), 0);
    if accounts_version <= MAX_ACCOUNTS_VERSION {
        for id in settings.child_groups(ACCOUNTS_GROUP) {
            let group = account_group(&id);
            let v = settings.i64_or(&join_key(&group, VERSION), 1);
            if v > MAX_ACCOUNT_VERSION {
                log::info!(target: "nextcloud.gui.account.manager", "Ignoring account {id} because of version {v}");
                ignore_keys.push(group);
            }
        }
    } else {
        delete_keys.push(ACCOUNTS_GROUP.to_owned());
    }
    (delete_keys, ignore_keys)
}

/// `AccountManager::AccountsRestoreResult` (without the legacy import).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreResult {
    Failure,
    Success,
    SuccessWithSkipped,
    NotFound,
}

/// What [`load_accounts`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountsLoad {
    pub result: RestoreResult,
    pub accounts: Vec<AccountDefinition>,
    /// Ids of accounts too new for this client (`_additionalBlockedAccountIds`).
    pub blocked_ids: Vec<String>,
}

/// `AccountManager::restore` (data part): every account of `[Accounts]`,
/// in `childGroups()` order. Accounts without URL are skipped like upstream.
/// The network limits are resolved against the client-wide settings
/// (`migrateNetworkSettings`).
pub fn load_accounts(settings: &Settings) -> AccountsLoad {
    let (delete_keys, ignore_keys) = backward_migration_settings_keys(settings);
    let mut out = AccountsLoad {
        result: RestoreResult::Success,
        accounts: Vec::new(),
        blocked_ids: Vec::new(),
    };
    if delete_keys.iter().any(|k| k == ACCOUNTS_GROUP) {
        log::warn!(target: "nextcloud.gui.account.manager", "Accounts structure is too new, ignoring");
        out.result = RestoreResult::SuccessWithSkipped;
        return out;
    }
    let ids = settings.child_groups(ACCOUNTS_GROUP);
    if ids.is_empty() {
        out.result = RestoreResult::NotFound;
        return out;
    }
    for id in ids {
        let group = account_group(&id);
        if ignore_keys.contains(&group) {
            log::info!(target: "nextcloud.gui.account.manager", "Account {id} is too new, ignoring");
            out.blocked_ids.push(id);
            out.result = RestoreResult::SuccessWithSkipped;
            continue;
        }
        if let Some(mut acc) = load_account(settings, &id) {
            migrate_network_settings(&mut acc, settings);
            out.accounts.push(acc);
        }
    }
    out
}

/// `AccountManager::loadAccountHelper` on `[Accounts] <id>\...`. `None`
/// when the account has no URL ("probably a corrupted entry").
pub fn load_account(settings: &Settings, id: &str) -> Option<AccountDefinition> {
    let group = account_group(id);
    let key = |k: &str| join_key(&group, k);
    let Some(url_config) = settings.value(&key(URL)).filter(|v| v.is_valid()) else {
        log::warn!(target: "nextcloud.gui.account.manager", "No URL for account {group}");
        return None;
    };
    let url_config = url_config.to_string_value();
    let mut acc = AccountDefinition {
        id: id.to_owned(),
        ..AccountDefinition::default()
    };
    let mut auth_type = settings.string(&key(AUTH_TYPE));

    // There was an account-type saving bug when 'skip folder config' was
    // used (owncloud#5408): fix up the "dummy" or empty authType.
    if auth_type == DUMMY_AUTH_TYPE || auth_type.is_empty() {
        if settings.contains(&key(HTTP_USER)) {
            auth_type = HTTP_AUTH_TYPE.to_owned();
        } else if settings.contains(&key(SHIBBOLETH_USER)) {
            auth_type = SHIBBOLETH_AUTH_TYPE.to_owned();
        } else if settings.contains(&key(WEBFLOW_USER)) {
            auth_type = WEBFLOW_AUTH_TYPE.to_owned();
        }
    }

    // Account::setUrl
    match public_share_link_parts(&url_config) {
        Some((base, _token)) => {
            acc.url = base;
            acc.public_share_link = Some(url_config.clone());
        }
        None => acc.url = url_config.clone(),
    }

    let child_keys = settings.child_keys(&group);
    if acc.public_share_link.is_some() {
        // Public share links always use HTTP basic authentication, even if
        // an earlier client saved them as webflow.
        if auth_type == WEBFLOW_AUTH_TYPE {
            auth_type = HTTP_AUTH_TYPE.to_owned();
            acc.credential_settings
                .insert(AUTH_TYPE.to_owned(), auth_type.clone());
            for k in &child_keys {
                if let Some(rest) = k.strip_prefix(WEBFLOW_AUTH_PREFIX) {
                    acc.credential_settings.insert(
                        format!("{HTTP_AUTH_PREFIX}{rest}"),
                        settings.string(&key(k)),
                    );
                }
            }
        }
    } else if auth_type == HTTP_AUTH_TYPE {
        // Migrate to webflow
        auth_type = WEBFLOW_AUTH_TYPE.to_owned();
        acc.credential_settings
            .insert(AUTH_TYPE.to_owned(), auth_type.clone());
        for k in &child_keys {
            if let Some(rest) = k.strip_prefix(HTTP_AUTH_PREFIX) {
                acc.credential_settings.insert(
                    format!("{WEBFLOW_AUTH_PREFIX}{rest}"),
                    settings.string(&key(k)),
                );
            }
        }
    }
    log::info!(target: "nextcloud.gui.account.manager", "Account for {} using auth type {auth_type}", acc.url);

    acc.server_version = settings.string(&key(SERVER_VERSION));
    // Assigned after setUrl, so a public share link without a stored
    // dav_user ends up with an empty one, like upstream.
    acc.dav_user = settings.string(&key(DAV_USER));
    if let Some(v) = settings.value(&key(USER)) {
        acc.credential_settings
            .insert(USER.to_owned(), v.to_string_value());
    }
    acc.display_name = settings.string(&key(DISPLAY_NAME));
    let prefix = format!("{auth_type}_");
    for k in &child_keys {
        if k.starts_with(&prefix) {
            acc.credential_settings
                .insert(k.clone(), settings.string(&key(k)));
        }
    }
    acc.auth_type = auth_type;

    let upload = LimitSetting::from_int(settings.i64_or(&key(NETWORK_UPLOAD_LIMIT_SETTING), 0));
    if upload == LimitSetting::AutoLimit {
        log::info!(target: "nextcloud.gui.account.manager", "Upload limit setting was set to deprecated auto limit, falling back to unlimited");
    }
    acc.set_upload_limit_setting(upload);
    let download = LimitSetting::from_int(settings.i64_or(&key(NETWORK_DOWNLOAD_LIMIT_SETTING), 0));
    if download == LimitSetting::AutoLimit {
        log::info!(target: "nextcloud.gui.account.manager", "Download limit setting was set to deprecated auto limit, falling back to unlimited");
    }
    acc.set_download_limit_setting(download);
    acc.upload_limit = settings.i64_or(&key(NETWORK_UPLOAD_LIMIT), 0);
    acc.download_limit = settings.i64_or(&key(NETWORK_DOWNLOAD_LIMIT), 0);

    acc.proxy = ProxySettings {
        proxy_type: settings.i64_or(&key(NETWORK_PROXY_TYPE), 0),
        host: settings.string(&key(NETWORK_PROXY_HOST_NAME)),
        port: settings.i64_or(&key(NETWORK_PROXY_PORT), 0),
        needs_auth: settings.bool_or(&key(NETWORK_PROXY_NEEDS_AUTH), false),
        user: settings.string(&key(NETWORK_PROXY_USER)),
    };
    acc.password_file = settings
        .value(&key(PASSWORD_FILE_KEY))
        .map(|v| v.to_string_value())
        .filter(|s| !s.is_empty());
    Some(acc)
}

/// `AccountManager::migrateNetworkSettings` (limits part): an account
/// stored with the legacy "use global settings" value takes the
/// client-wide `[BWLimit]` values.
pub fn migrate_network_settings(acc: &mut AccountDefinition, settings: &Settings) {
    let group = account_group(&acc.id);
    let cfg = ConfigFile::new(settings);
    let mut use_up = LimitSetting::from_int(settings.i64_or(
        &join_key(&group, NETWORK_UPLOAD_LIMIT_SETTING),
        acc.upload_limit_setting.to_int(),
    ));
    let mut up = settings.i64_or(&join_key(&group, NETWORK_UPLOAD_LIMIT), acc.upload_limit);
    let mut use_down = LimitSetting::from_int(settings.i64_or(
        &join_key(&group, NETWORK_DOWNLOAD_LIMIT_SETTING),
        acc.download_limit_setting.to_int(),
    ));
    let mut down = settings.i64_or(
        &join_key(&group, NETWORK_DOWNLOAD_LIMIT),
        acc.download_limit,
    );
    if use_up == LimitSetting::LegacyGlobalLimit {
        use_up = LimitSetting::from_int(cfg.use_upload_limit());
        up = cfg.upload_limit();
    }
    if use_down == LimitSetting::LegacyGlobalLimit {
        use_down = LimitSetting::from_int(cfg.use_download_limit());
        down = cfg.download_limit();
    }
    if use_up != LimitSetting::NoLimit {
        acc.set_upload_limit_setting(use_up);
        acc.upload_limit = up;
    }
    if use_down != LimitSetting::NoLimit {
        acc.set_download_limit_setting(use_down);
        acc.download_limit = down;
    }
}

/// `AccountManager::saveAccountHelper` (data part, without the keychain and
/// cookies), plus `[Accounts] version` as `save()` writes it.
pub fn save_account(settings: &mut Settings, acc: &AccountDefinition) {
    settings.set_value(&join_key(ACCOUNTS_GROUP, VERSION), MAX_ACCOUNTS_VERSION);
    let group = account_group(&acc.id);
    let key = |k: &str| join_key(&group, k);
    settings.set_value(&key(VERSION), MAX_ACCOUNT_VERSION);
    let url = acc.public_share_link.as_ref().unwrap_or(&acc.url);
    settings.set_value(&key(URL), url);
    settings.set_value(&key(DAV_USER), &acc.dav_user);
    settings.set_value(&key(DISPLAY_NAME), &acc.display_name);
    settings.set_value(&key(SERVER_VERSION), &acc.server_version);
    settings.set_value(&key(NETWORK_PROXY_TYPE), acc.proxy.proxy_type);
    settings.set_value(&key(NETWORK_PROXY_HOST_NAME), &acc.proxy.host);
    settings.set_value(&key(NETWORK_PROXY_PORT), acc.proxy.port);
    settings.set_value(&key(NETWORK_PROXY_NEEDS_AUTH), acc.proxy.needs_auth);
    settings.set_value(&key(NETWORK_PROXY_USER), &acc.proxy.user);
    settings.set_value(
        &key(NETWORK_UPLOAD_LIMIT_SETTING),
        acc.upload_limit_setting.to_int(),
    );
    settings.set_value(
        &key(NETWORK_DOWNLOAD_LIMIT_SETTING),
        acc.download_limit_setting.to_int(),
    );
    settings.set_value(&key(NETWORK_UPLOAD_LIMIT), acc.upload_limit);
    settings.set_value(&key(NETWORK_DOWNLOAD_LIMIT), acc.download_limit);
    for (k, v) in &acc.credential_settings {
        settings.set_value(&key(k), v);
    }
    settings.set_value(&key(AUTH_TYPE), &acc.auth_type);
    // HACK: Save http_user also as user
    if let Some(v) = acc.credential_settings.get(HTTP_USER) {
        settings.set_value(&key(USER), v);
    }
    match &acc.password_file {
        Some(p) => settings.set_value(&key(PASSWORD_FILE_KEY), p),
        None => settings.remove(&key(PASSWORD_FILE_KEY)),
    }
}

/// `AccountManager::removeAccountState` (settings part): drops the group.
pub fn remove_account(settings: &mut Settings, id: &str) {
    settings.remove(&account_group(id));
}

/// `AccountManager::generateFreeAccountId`: the smallest number not used by
/// an account group nor blocked.
pub fn generate_free_account_id(settings: &Settings, blocked: &[String]) -> String {
    let used = settings.child_groups(ACCOUNTS_GROUP);
    (0..)
        .map(|i: u64| i.to_string())
        .find(|id| !used.contains(id) && !blocked.contains(id))
        .unwrap_or_default()
}

/// Finds an account by server URL and user (the same account as seen by
/// the server: same `url` and same login name).
pub fn find_account<'a>(
    accounts: &'a [AccountDefinition],
    url: &str,
    user: &str,
) -> Option<&'a AccountDefinition> {
    let norm = |u: &str| u.trim_end_matches('/').to_owned();
    accounts
        .iter()
        .find(|a| norm(&a.url) == norm(url) && a.credentials_user() == user)
}

#[cfg(test)]
mod tests;
