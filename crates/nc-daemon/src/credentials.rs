// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `AbstractCredentials::keychainKey`
// (`src/libsync/creds/abstractcredentials.cpp`) and of the keychain key
// layout of `WebFlowCredentials` (`src/gui/creds/webflowcredentials.cpp`) and
// `Account::writeAppPasswordOnce` (`src/libsync/account.cpp`)
// (nextcloud/desktop v34.0.5). The storage back ends (Secret Service through
// keyring-core, systemd credentials, password files) are this port's own.

//! Where an account's app password comes from.
//!
//! Resolution order for [`CredentialResolver::app_password`]:
//!
//! 1. systemd credentials, when the daemon runs with `CREDENTIALS_DIRECTORY`
//!    set: `$CREDENTIALS_DIRECTORY/ncsyncd-<user>-<accountId>` for the system
//!    instance `ncsyncd@<user>` (its unit imports them from the credential
//!    store with `ImportCredential=ncsyncd-%i-*`: `/etc/credstore`,
//!    `/etc/credstore.encrypted`, ...; the user name in the credential name
//!    keeps one user's instance from receiving another user's secrets),
//!    `$CREDENTIALS_DIRECTORY/ncsyncd-<accountId>` for the user daemon;
//! 2. the account's `ncsyncd_passwordFile` key (a file whose first line is
//!    the app password);
//! 3. for the user daemon only, the Secret Service (keyring): service
//!    `ncsyncd`, user = the upstream keychain key
//!    `<login name>:<server url>/:<accountId>`.
//!
//! The official client keeps its app password with QtKeychain under the
//! service (Secret Service attribute `server`) `Nextcloud` and the same key
//! layout (attribute `user`); [`official_client_app_password`] reads it, read
//! only and best effort, for a takeover.
//!
//! Secrets are wrapped in [`AppPassword`], whose `Debug` does not show them,
//! and are never logged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;

use crate::account_config::AccountDefinition;
use crate::config_file::{APP_NAME, ServiceMode};

/// Our Secret Service / keyring service name.
pub const KEYRING_SERVICE: &str = "ncsyncd";
/// The prefix of systemd credential names (see [`systemd_credential_name`]).
pub const SYSTEMD_CREDENTIAL_PREFIX: &str = "ncsyncd-";
/// `app_password` of account.cpp: suffix of the user part of the key under
/// which `Account::writeAppPasswordOnce` keeps a copy of the app password.
pub const APP_PASSWORD_SUFFIX: &str = "_app-password";

/// An app password. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct AppPassword(String);

impl AppPassword {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself, to build the request credentials.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `nc_dav::Credentials` for basic auth.
    pub fn to_dav_credentials(&self, user: &str) -> nc_dav::Credentials {
        nc_dav::Credentials::new(user, self.0.clone())
    }
}

impl std::fmt::Debug for AppPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppPassword(<redacted>)")
    }
}

/// Errors about credentials.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("the keyring (Secret Service) is not available: {0}")]
    StoreUnavailable(String),
    #[error("keyring error: {0}")]
    Store(String),
    #[error("cannot read the password file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write the password file {path}: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("account {account}: no keychain key (empty server URL or login name)")]
    NoKey { account: String },
    #[error(
        "no app password for account {account} ({user}): tried {tried}; add one with `ncsync account add`"
    )]
    NotFound {
        account: String,
        user: String,
        tried: String,
    },
}

/// `AbstractCredentials::keychainKey(url, user, accountId)`:
/// `<user>:<url with trailing />[:<accountId>]`. `None` when the URL or the
/// user is empty (upstream logs an error and returns an empty key).
pub fn keychain_key(url: &str, user: &str, account_id: &str) -> Option<String> {
    if url.is_empty() {
        log::warn!(target: "nextcloud.sync.credentials", "Empty url in keyChain, error!");
        return None;
    }
    if user.is_empty() {
        log::warn!(target: "nextcloud.sync.credentials", "Error: User is empty!");
        return None;
    }
    let mut server_url = url.to_owned();
    if !server_url.ends_with('/') {
        server_url.push('/');
    }
    let mut key = format!("{user}:{server_url}");
    if !account_id.is_empty() {
        key.push(':');
        key.push_str(account_id);
    }
    Some(key)
}

/// The keychain key of an account's password (`WebFlowCredentials`).
pub fn account_keychain_key(acc: &AccountDefinition) -> Option<String> {
    keychain_key(&acc.url_string(), &acc.credentials_user(), &acc.id)
}

/// The systemd credential name of an account: `ncsyncd-<user>-<accountId>`
/// for the system instance `ncsyncd@<user>`, `ncsyncd-<accountId>` for the
/// user daemon.
pub fn systemd_credential_name(mode: &ServiceMode, account_id: &str) -> String {
    match mode {
        ServiceMode::System { instance } => {
            format!("{SYSTEMD_CREDENTIAL_PREFIX}{instance}-{account_id}")
        }
        ServiceMode::User => format!("{SYSTEMD_CREDENTIAL_PREFIX}{account_id}"),
    }
}

/// The name of the password file `account add` writes when there is no
/// keyring: `ncsyncd-<accountId>`, in the per-user state directory.
pub fn password_file_name(account_id: &str) -> String {
    format!("{SYSTEMD_CREDENTIAL_PREFIX}{account_id}")
}

/// A secret found by an attribute search.
#[derive(Clone)]
pub struct FoundSecret {
    pub attributes: HashMap<String, String>,
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for FoundSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FoundSecret")
            .field("attributes", &self.attributes)
            .finish_non_exhaustive()
    }
}

/// A secret store: the Secret Service in production, [`MemoryStore`] in
/// tests.
pub trait SecretStore: Send + Sync {
    /// The password of (`service`, `user`), `None` when there is none.
    fn get(&self, service: &str, user: &str) -> Result<Option<String>, CredentialsError>;
    /// Creates or replaces the password of (`service`, `user`).
    fn set(&self, service: &str, user: &str, secret: &str) -> Result<(), CredentialsError>;
    /// Deletes it; `false` when there was none.
    fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialsError>;
    /// Items whose attributes contain all of `attributes` (Secret Service
    /// search, used to read QtKeychain items).
    fn search(&self, attributes: &[(&str, &str)]) -> Result<Vec<FoundSecret>, CredentialsError>;
}

fn store_error(e: keyring_core::Error) -> CredentialsError {
    match e {
        keyring_core::Error::NoStorageAccess(p) => {
            CredentialsError::StoreUnavailable(p.to_string())
        }
        e => CredentialsError::Store(e.to_string()),
    }
}

/// The keyring-core based store.
pub struct KeyringStore {
    store: Arc<keyring_core::CredentialStore>,
}

impl KeyringStore {
    /// The Secret Service of the session (gnome-keyring, KWallet's
    /// Secret Service API, KeePassXC, ...), via D-Bus.
    pub fn secret_service() -> Result<Self, CredentialsError> {
        let store = zbus_secret_service_keyring_store::Store::new()
            .map_err(|e| CredentialsError::StoreUnavailable(e.to_string()))?;
        Ok(Self { store })
    }

    /// Any keyring-core store.
    pub fn from_store(store: Arc<keyring_core::CredentialStore>) -> Self {
        Self { store }
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, service: &str, user: &str) -> Result<Option<String>, CredentialsError> {
        let entry = self.store.build(service, user, None).map_err(store_error)?;
        match entry.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    fn set(&self, service: &str, user: &str, secret: &str) -> Result<(), CredentialsError> {
        let entry = self.store.build(service, user, None).map_err(store_error)?;
        entry.set_password(secret).map_err(store_error)
    }

    fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialsError> {
        let entry = self.store.build(service, user, None).map_err(store_error)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(store_error(e)),
        }
    }

    fn search(&self, attributes: &[(&str, &str)]) -> Result<Vec<FoundSecret>, CredentialsError> {
        let spec: HashMap<&str, &str> = attributes.iter().copied().collect();
        let entries = self.store.search(&spec).map_err(store_error)?;
        let mut out = Vec::new();
        for e in entries {
            let attributes = e.get_attributes().map_err(store_error)?;
            let secret = e.get_secret().map_err(store_error)?;
            out.push(FoundSecret { attributes, secret });
        }
        Ok(out)
    }
}

/// An in-memory store with Secret Service-like attributes (`service` and
/// `username` for keyring entries), for tests.
#[derive(Default)]
pub struct MemoryStore {
    items: Mutex<Vec<FoundSecret>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an item with arbitrary attributes (e.g. a QtKeychain one).
    pub fn insert(&self, attributes: &[(&str, &str)], secret: &[u8]) {
        let attributes = attributes
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.lock().push(FoundSecret {
            attributes,
            secret: secret.to_vec(),
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<FoundSecret>> {
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn matches(item: &FoundSecret, attributes: &[(&str, &str)]) -> bool {
        attributes
            .iter()
            .all(|(k, v)| item.attributes.get(*k).is_some_and(|x| x == v))
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, service: &str, user: &str) -> Result<Option<String>, CredentialsError> {
        let spec = [("service", service), ("username", user)];
        Ok(self
            .lock()
            .iter()
            .find(|i| Self::matches(i, &spec))
            .map(|i| String::from_utf8_lossy(&i.secret).into_owned()))
    }

    fn set(&self, service: &str, user: &str, secret: &str) -> Result<(), CredentialsError> {
        self.delete(service, user)?;
        self.insert(
            &[("service", service), ("username", user)],
            secret.as_bytes(),
        );
        Ok(())
    }

    fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialsError> {
        let spec = [("service", service), ("username", user)];
        let mut items = self.lock();
        let before = items.len();
        items.retain(|i| !Self::matches(i, &spec));
        Ok(items.len() != before)
    }

    fn search(&self, attributes: &[(&str, &str)]) -> Result<Vec<FoundSecret>, CredentialsError> {
        Ok(self
            .lock()
            .iter()
            .filter(|i| Self::matches(i, attributes))
            .cloned()
            .collect())
    }
}

/// Where a secret came from (for messages; never the secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    SystemdCredential(PathBuf),
    PasswordFile(PathBuf),
    Keyring { service: String, key: String },
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemdCredential(p) => write!(f, "systemd credential {}", p.display()),
            Self::PasswordFile(p) => write!(f, "password file {}", p.display()),
            Self::Keyring { service, key } => write!(f, "keyring entry {service}/{key}"),
        }
    }
}

/// Reads the first line of a secret file (a trailing newline is not part of
/// the secret).
fn read_secret_file(path: &Path) -> Result<AppPassword, CredentialsError> {
    let text = std::fs::read_to_string(path).map_err(|source| CredentialsError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    Ok(AppPassword::new(text.lines().next().unwrap_or("")))
}

/// Finds and stores app passwords for the daemon's accounts.
pub struct CredentialResolver<'a> {
    pub mode: ServiceMode,
    /// The keyring; `None` for a system instance or when unavailable.
    pub store: Option<&'a dyn SecretStore>,
    /// `$CREDENTIALS_DIRECTORY`.
    pub credentials_directory: Option<PathBuf>,
}

impl<'a> CredentialResolver<'a> {
    /// A resolver with `$CREDENTIALS_DIRECTORY` from the environment.
    pub fn new(mode: ServiceMode, store: Option<&'a dyn SecretStore>) -> Self {
        let credentials_directory = std::env::var_os("CREDENTIALS_DIRECTORY")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Self {
            mode,
            store,
            credentials_directory,
        }
    }

    fn keyring_allowed(&self) -> bool {
        self.mode == ServiceMode::User
    }

    /// The app password of an account, and where it was found.
    pub fn app_password(
        &self,
        acc: &AccountDefinition,
    ) -> Result<(AppPassword, CredentialSource), CredentialsError> {
        let mut tried = Vec::new();
        if let Some(dir) = &self.credentials_directory {
            let path = dir.join(systemd_credential_name(&self.mode, &acc.id));
            if path.exists() {
                return Ok((
                    read_secret_file(&path)?,
                    CredentialSource::SystemdCredential(path),
                ));
            }
            tried.push(format!("systemd credential {}", path.display()));
        }
        if let Some(file) = &acc.password_file {
            let path = PathBuf::from(file);
            return Ok((
                read_secret_file(&path)?,
                CredentialSource::PasswordFile(path),
            ));
        }
        if self.keyring_allowed()
            && let Some(store) = self.store
        {
            let key = account_keychain_key(acc).ok_or_else(|| CredentialsError::NoKey {
                account: acc.id.clone(),
            })?;
            if let Some(p) = store.get(KEYRING_SERVICE, &key)? {
                return Ok((
                    AppPassword::new(p),
                    CredentialSource::Keyring {
                        service: KEYRING_SERVICE.to_owned(),
                        key,
                    },
                ));
            }
            tried.push(format!("keyring {KEYRING_SERVICE}/{key}"));
        }
        if tried.is_empty() {
            tried.push("nothing (no keyring for a system instance)".to_owned());
        }
        Err(CredentialsError::NotFound {
            account: acc.id.clone(),
            user: acc.credentials_user(),
            tried: tried.join(", "),
        })
    }

    /// Stores a new app password: in the keyring for the user daemon (when
    /// there is one); for a system instance or without a keyring, in `<state_dir>/credentials/ncsyncd-<id>` (mode
    /// 0600), whose path is put in `acc.password_file` (the caller saves the
    /// account). systemd credentials are provisioned by the administrator.
    pub fn store_app_password(
        &self,
        acc: &mut AccountDefinition,
        secret: &AppPassword,
        state_dir: &Path,
    ) -> Result<CredentialSource, CredentialsError> {
        if self.keyring_allowed()
            && let Some(store) = self.store
        {
            let key = account_keychain_key(acc).ok_or_else(|| CredentialsError::NoKey {
                account: acc.id.clone(),
            })?;
            store.set(KEYRING_SERVICE, &key, secret.expose())?;
            acc.password_file = None;
            return Ok(CredentialSource::Keyring {
                service: KEYRING_SERVICE.to_owned(),
                key,
            });
        }
        if self.keyring_allowed() {
            // A user daemon without a Secret Service (a headless server):
            // the secret goes to a password file, like for a system instance.
            log::warn!(
                "No keyring available: storing the app password of account {} in a file of {} (mode 0600)",
                acc.id,
                state_dir.display()
            );
        }
        let dir = state_dir.join("credentials");
        let path = dir.join(password_file_name(&acc.id));
        write_secret_file(&dir, &path, secret)?;
        acc.password_file = Some(path.to_string_lossy().into_owned());
        Ok(CredentialSource::PasswordFile(path))
    }

    /// Deletes what [`Self::store_app_password`] wrote. Systemd credentials
    /// are left to the administrator.
    pub fn forget(&self, acc: &AccountDefinition) -> Result<(), CredentialsError> {
        if let Some(file) = &acc.password_file {
            let _ = std::fs::remove_file(file);
        }
        if self.keyring_allowed()
            && let Some(store) = self.store
            && let Some(key) = account_keychain_key(acc)
        {
            store.delete(KEYRING_SERVICE, &key)?;
        }
        Ok(())
    }
}

fn write_secret_file(
    dir: &Path,
    path: &Path,
    secret: &AppPassword,
) -> Result<(), CredentialsError> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let werr = |source| CredentialsError::WriteFile {
        path: path.to_owned(),
        source,
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(werr)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(werr)?;
    writeln!(f, "{}", secret.expose()).map_err(werr)
}

/// Reads the official client's app password for `acc` (an account of the
/// official configuration, with its own id) from the Secret Service, as
/// QtKeychain stored it: attribute `server` = `Nextcloud` (the app name),
/// attribute `user` = the keychain key, attribute `type` = `plaintext` or
/// `base64`. Tried in order: the `WebFlowCredentials` password
/// (`<user>:<url>/:<id>`), the copy written by `writeAppPasswordOnce`
/// (`<davUser>_app-password:<url>/:<id>`), and the pre-2.4 key without
/// account id. Read only; `None` when nothing is found.
///
/// Best effort: it only works when the official client uses QtKeychain's
/// libsecret back end (GNOME, and KDE with the Secret Service API); with
/// the direct KWallet back end the secret is not visible here.
pub fn official_client_app_password(
    store: &dyn SecretStore,
    acc: &AccountDefinition,
) -> Result<Option<AppPassword>, CredentialsError> {
    let url = acc.url_string();
    let user = acc.credentials_user();
    let candidates = [
        keychain_key(&url, &user, &acc.id),
        keychain_key(
            &url,
            &format!("{}{APP_PASSWORD_SUFFIX}", acc.effective_dav_user()),
            &acc.id,
        ),
        keychain_key(&url, &user, ""),
    ];
    for key in candidates.into_iter().flatten() {
        let found = store.search(&[("user", key.as_str()), ("server", APP_NAME)])?;
        if let Some(item) = found.into_iter().next() {
            let is_base64 = item.attributes.get("type").is_some_and(|t| t == "base64");
            let bytes = if is_base64 {
                base64::engine::general_purpose::STANDARD
                    .decode(item.secret.trim_ascii())
                    .unwrap_or_default()
            } else {
                item.secret
            };
            let secret = String::from_utf8_lossy(&bytes).into_owned();
            if !secret.is_empty() {
                log::info!(target: "nextcloud.sync.credentials", "Found the official client's app password for account {} ({user})", acc.id);
                return Ok(Some(AppPassword::new(secret)));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountDefinition {
        AccountDefinition::new_webflow("3", "https://cloud.example.com", "alice")
    }

    #[test]
    fn derived_keychain_key_layout() {
        assert_eq!(
            keychain_key("https://cloud.example.com", "alice", "0").as_deref(),
            Some("alice:https://cloud.example.com/:0")
        );
        assert_eq!(
            keychain_key("https://h/nc/", "bob", "").as_deref(),
            Some("bob:https://h/nc/")
        );
        assert_eq!(keychain_key("", "bob", "0"), None);
        assert_eq!(keychain_key("https://h", "", "0"), None);
        assert_eq!(
            systemd_credential_name(&ServiceMode::User, "0"),
            "ncsyncd-0"
        );
        assert_eq!(
            systemd_credential_name(
                &ServiceMode::System {
                    instance: "alice".into()
                },
                "0"
            ),
            "ncsyncd-alice-0"
        );
        assert_eq!(password_file_name("0"), "ncsyncd-0");
        assert_eq!(
            format!("{:?}", AppPassword::new("s3cret")),
            "AppPassword(<redacted>)"
        );
    }

    #[test]
    fn derived_resolution_order() {
        let store = MemoryStore::new();
        let dir = tempfile::tempdir().unwrap();
        let mut acc = account();
        let user = CredentialResolver {
            mode: ServiceMode::User,
            store: Some(&store),
            credentials_directory: None,
        };
        assert!(matches!(
            user.app_password(&acc),
            Err(CredentialsError::NotFound { .. })
        ));
        let src = user
            .store_app_password(&mut acc, &AppPassword::new("kr"), dir.path())
            .unwrap();
        assert_eq!(
            src,
            CredentialSource::Keyring {
                service: "ncsyncd".into(),
                key: "alice:https://cloud.example.com/:3".into()
            }
        );
        assert_eq!(user.app_password(&acc).unwrap().0.expose(), "kr");

        // A password file wins over the keyring.
        let pf = dir.path().join("pw");
        std::fs::write(&pf, "file-secret\nignored\n").unwrap();
        acc.password_file = Some(pf.to_string_lossy().into_owned());
        let (p, s) = user.app_password(&acc).unwrap();
        assert_eq!(
            (p.expose(), s),
            ("file-secret", CredentialSource::PasswordFile(pf.clone()))
        );

        // A systemd credential wins over both.
        let creds = dir.path().join("creds");
        std::fs::create_dir(&creds).unwrap();
        std::fs::write(creds.join("ncsyncd-3"), "sd\n").unwrap();
        let with_sd = CredentialResolver {
            credentials_directory: Some(creds.clone()),
            ..user
        };
        let (p, s) = with_sd.app_password(&acc).unwrap();
        assert_eq!(p.expose(), "sd");
        assert_eq!(
            s,
            CredentialSource::SystemdCredential(creds.join("ncsyncd-3"))
        );

        with_sd.forget(&acc).unwrap();
        assert!(!pf.exists());
        assert_eq!(
            store
                .get("ncsyncd", "alice:https://cloud.example.com/:3")
                .unwrap(),
            None
        );
    }

    #[test]
    fn derived_system_instance_never_uses_the_keyring() {
        let store = MemoryStore::new();
        let dir = tempfile::tempdir().unwrap();
        let mut acc = account();
        store
            .set("ncsyncd", "alice:https://cloud.example.com/:3", "kr")
            .unwrap();
        let sys = CredentialResolver {
            mode: ServiceMode::System {
                instance: "x".into(),
            },
            store: Some(&store),
            credentials_directory: None,
        };
        assert!(sys.app_password(&acc).is_err());
        // The system instance reads ncsyncd-<instance>-<id> only.
        let creds = dir.path().join("creds");
        std::fs::create_dir(&creds).unwrap();
        std::fs::write(creds.join("ncsyncd-3"), "other\n").unwrap();
        let sys_sd = CredentialResolver {
            mode: sys.mode.clone(),
            store: None,
            credentials_directory: Some(creds.clone()),
        };
        assert!(sys_sd.app_password(&acc).is_err());
        std::fs::write(creds.join("ncsyncd-x-3"), "mine\n").unwrap();
        assert_eq!(
            sys_sd.app_password(&acc).unwrap(),
            (
                AppPassword::new("mine"),
                CredentialSource::SystemdCredential(creds.join("ncsyncd-x-3"))
            )
        );
        let src = sys
            .store_app_password(&mut acc, &AppPassword::new("pw"), dir.path())
            .unwrap();
        let path = dir.path().join("credentials/ncsyncd-3");
        assert_eq!(src, CredentialSource::PasswordFile(path.clone()));
        assert_eq!(acc.password_file.as_deref(), Some(path.to_str().unwrap()));
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(sys.app_password(&acc).unwrap().0.expose(), "pw");
    }

    #[test]
    fn derived_official_client_lookup() {
        let store = MemoryStore::new();
        let mut acc = account();
        acc.id = "0".into();
        assert!(
            official_client_app_password(&store, &acc)
                .unwrap()
                .is_none()
        );
        // writeAppPasswordOnce's binary copy (QtKeychain base64 type).
        store.insert(
            &[
                ("user", "alice_app-password:https://cloud.example.com/:0"),
                ("server", "Nextcloud"),
                ("type", "base64"),
            ],
            b"YmluYXJ5LWNvcHk=",
        );
        assert_eq!(
            official_client_app_password(&store, &acc)
                .unwrap()
                .unwrap()
                .expose(),
            "binary-copy"
        );
        // The WebFlowCredentials password is preferred.
        store.insert(
            &[
                ("user", "alice:https://cloud.example.com/:0"),
                ("server", "Nextcloud"),
                ("type", "plaintext"),
            ],
            b"webflow-pw",
        );
        assert_eq!(
            official_client_app_password(&store, &acc)
                .unwrap()
                .unwrap()
                .expose(),
            "webflow-pw"
        );
        // Another application's item with the same user is not taken.
        let other = MemoryStore::new();
        other.insert(
            &[
                ("user", "alice:https://cloud.example.com/:0"),
                ("server", "Other"),
            ],
            b"x",
        );
        assert!(
            official_client_app_password(&other, &acc)
                .unwrap()
                .is_none()
        );
    }

    /// Real Secret Service round trip (run by hand: it writes, reads and
    /// deletes one `ncsyncd` test entry in the session keyring).
    #[test]
    #[ignore = "needs a session Secret Service; writes a test entry"]
    fn derived_secret_service_round_trip() {
        let store = KeyringStore::secret_service().unwrap();
        let key = "ncsyncd-test:https://example.invalid/:999";
        store.set(KEYRING_SERVICE, key, "test-secret").unwrap();
        assert_eq!(
            store.get(KEYRING_SERVICE, key).unwrap().as_deref(),
            Some("test-secret")
        );
        assert!(store.delete(KEYRING_SERVICE, key).unwrap());
        assert_eq!(store.get(KEYRING_SERVICE, key).unwrap(), None);
    }
}
