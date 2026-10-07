// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The configuration commands of `ncsync` (`account add|list`,
//! `folder add|list|remove`, `takeover`, `handback`), on top of the other
//! modules. Printing is left to the binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nc_dav::{Account, HttpClientOptions, HttpTransport, JobOptions, ServerUrl, Transport};

use crate::account_config::{
    AccountDefinition, find_account, generate_free_account_id, load_accounts, remove_account,
    save_account,
};
use crate::config_file::{ConfigLocation, ServiceMode};
use crate::credentials::{
    AppPassword, CredentialResolver, CredentialSource, CredentialsError, KeyringStore, SecretStore,
    official_client_app_password,
};
use crate::flow2auth::{self, Flow2Error, LoginFlow, LoginResult};
use crate::folder_definition::{
    ExistingFolder, FolderDefinition, ServerIdentity, all_aliases, allocate_alias,
    check_path_validity_for_new_folder, choose_group, clean_path, folder_canonical_path,
    load_folders, save_folder,
};
use crate::settings::{Settings, SettingsError};
use crate::takeover::{
    self, HandbackOutcome, TakeoverError, TakeoverOutcome, handback_source, taken_over_aliases,
};

/// Errors of the configuration commands.
#[derive(Debug, thiserror::Error)]
pub enum ManageError {
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    #[error(transparent)]
    Login(#[from] Flow2Error),
    #[error(transparent)]
    Takeover(#[from] TakeoverError),
    #[error("invalid server URL {0:?}")]
    InvalidUrl(String),
    #[error("{0}")]
    Server(String),
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Io(String),
}

/// The configuration a command works on.
#[derive(Clone, Debug)]
pub struct Context {
    pub location: ConfigLocation,
    pub http: HttpClientOptions,
}

impl Context {
    pub fn load(&self) -> Result<Settings, ManageError> {
        Ok(Settings::load(&self.location.config_file)?)
    }

    fn transport(&self) -> Result<Arc<dyn Transport>, ManageError> {
        let t = HttpTransport::new(&self.http).map_err(|e| ManageError::Server(e.to_string()))?;
        Ok(Arc::new(t))
    }

    /// The keyring, for the user daemon (unavailable keyrings are an error
    /// only when a secret has to be read or written there).
    pub fn secret_store(&self) -> Option<KeyringStore> {
        match self.location.mode {
            ServiceMode::User => match KeyringStore::secret_service() {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!("{e}");
                    None
                }
            },
            ServiceMode::System { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// An account, for `account list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSummary {
    pub id: String,
    pub url: String,
    pub user: String,
    pub dav_user: String,
    pub display_name: String,
    pub server_version: String,
    pub auth_type: String,
}

/// A folder, for `folder list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSummary {
    pub alias: String,
    pub account_id: String,
    pub local_path: String,
    pub remote_path: String,
    pub journal_path: String,
    pub paused: bool,
    pub group: &'static str,
    pub virtual_files_mode: &'static str,
    /// Taken over from the official client (can be handed back).
    pub taken_over: bool,
}

pub fn list_accounts(settings: &Settings) -> Vec<AccountSummary> {
    load_accounts(settings)
        .accounts
        .into_iter()
        .map(|a| AccountSummary {
            user: a.credentials_user(),
            dav_user: a.effective_dav_user(),
            id: a.id,
            url: a.public_share_link.unwrap_or(a.url),
            display_name: a.display_name,
            server_version: a.server_version,
            auth_type: a.auth_type,
        })
        .collect()
}

pub fn list_folders(settings: &Settings) -> Vec<FolderSummary> {
    let accounts = load_accounts(settings).accounts;
    let taken = taken_over_aliases(settings);
    load_folders(settings, &accounts)
        .folders
        .into_iter()
        .map(|f| FolderSummary {
            taken_over: taken.contains(&f.definition.alias),
            alias: f.definition.alias,
            account_id: f.account_id,
            local_path: f.definition.local_path,
            remote_path: f.definition.target_path,
            journal_path: f.definition.journal_path,
            paused: f.definition.paused,
            group: f.group.name(),
            virtual_files_mode: f.definition.virtual_files_mode.as_str(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// What the server says about an account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteAccountInfo {
    pub server_version: String,
    /// The user id (`ocs/v1.php/cloud/user` `id`), the DAV user.
    pub user_id: String,
    pub display_name: String,
}

fn parse_server_url(url: &str) -> Result<ServerUrl, ManageError> {
    let trimmed = url.trim().trim_end_matches(['/', '\\']);
    ServerUrl::parse(trimmed).map_err(|_| ManageError::InvalidUrl(url.to_owned()))
}

/// `status.php` (server version), then the user with the credentials
/// (user id and display name), like the connection validator.
pub async fn fetch_account_info(
    transport: Arc<dyn Transport>,
    server_url: &str,
    login_name: &str,
    app_password: &AppPassword,
) -> Result<RemoteAccountInfo, ManageError> {
    let url = parse_server_url(server_url)?;
    let account = Account::new(url, transport);
    let opts = JobOptions::default();
    let mut info = RemoteAccountInfo::default();
    match nc_dav::jobs::check_server(&account, &opts).await {
        Ok(status) => {
            info.server_version = status
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            if status.get("maintenance").and_then(|v| v.as_bool()) == Some(true) {
                return Err(ManageError::Server(
                    "the server is in maintenance mode".to_owned(),
                ));
            }
        }
        Err(reply) => {
            return Err(ManageError::Server(format!(
                "{server_url} is not a Nextcloud server ({})",
                reply.error_string()
            )));
        }
    }
    account.set_credentials(app_password.to_dav_credentials(login_name));
    let (json, _status, reply) =
        nc_dav::jobs::json_api(&account, "ocs/v1.php/cloud/user", &opts).await;
    if !reply.is_ok() {
        return Err(ManageError::Server(if reply.http_status == 401 {
            format!("the server refused the login {login_name:?} with this app password")
        } else {
            format!("cannot read the user: {}", reply.error_string())
        }));
    }
    let data = json.as_ref().map(|j| &j["ocs"]["data"]);
    info.user_id = data
        .and_then(|d| d["id"].as_str())
        .unwrap_or(login_name)
        .to_owned();
    info.display_name = data
        .and_then(|d| d["display-name"].as_str())
        .unwrap_or_default()
        .to_owned();
    Ok(info)
}

/// Starts Login Flow v2 on `server_url`; the caller shows
/// [`LoginFlow::login_url`] (and [`flow2auth::render_qr`]) then calls
/// [`finish_login`].
pub async fn start_login(ctx: &Context, server_url: &str) -> Result<LoginFlow, ManageError> {
    let url = parse_server_url(server_url)?;
    let transport = ctx.transport()?;
    let account = Account::new(url, transport);
    // The server version decides whether the login name can be prefilled.
    if let Ok(status) = nc_dav::jobs::check_server(&account, &JobOptions::default()).await
        && let Some(v) = status.get("version").and_then(|v| v.as_str())
    {
        account.set_server_version(v);
    }
    Ok(flow2auth::start_login(&account, None).await?)
}

/// Polls a started login flow until it completes.
pub async fn finish_login(
    ctx: &Context,
    flow: &LoginFlow,
    on_error: impl FnMut(&Flow2Error),
) -> Result<LoginResult, ManageError> {
    let transport = ctx.transport()?;
    Ok(
        flow2auth::wait_for_login(&transport, flow, flow2auth::DEFAULT_LOGIN_TIMEOUT, on_error)
            .await?,
    )
}

/// The result of `account add`.
#[derive(Debug)]
pub struct AddedAccount {
    pub account: AccountDefinition,
    /// It was already configured (only its credentials were replaced).
    pub existed: bool,
    pub credentials: CredentialSource,
}

/// `account add`: checks the login with the server, saves the account
/// (or updates the one with the same server and login name) and stores the
/// app password (keyring, or a password file for a system instance).
pub async fn add_account(
    ctx: &Context,
    server_url: &str,
    login_name: &str,
    app_password: &AppPassword,
    store: Option<&dyn SecretStore>,
) -> Result<AddedAccount, ManageError> {
    let url = parse_server_url(server_url)?.to_credential_free_string();
    let info = fetch_account_info(ctx.transport()?, &url, login_name, app_password).await?;
    let path = ctx.location.config_file.clone();
    let ((def, existed), _) = Settings::modify(&path, |s| {
        let load = load_accounts(s);
        let (mut def, existed) = match find_account(&load.accounts, &url, login_name) {
            Some(a) => (a.clone(), true),
            None => {
                let id = generate_free_account_id(s, &load.blocked_ids);
                (AccountDefinition::new_webflow(&id, &url, login_name), false)
            }
        };
        def.dav_user = info.user_id.clone();
        def.display_name = info.display_name.clone();
        def.server_version = info.server_version.clone();
        save_account(s, &def);
        (def, existed)
    })?;
    let mut def = def;
    let resolver = CredentialResolver::new(ctx.location.mode.clone(), store);
    match resolver.store_app_password(&mut def, app_password, &ctx.location.state_dir) {
        Ok(source) => {
            Settings::modify(&path, |s| save_account(s, &def))?;
            Ok(AddedAccount {
                account: def,
                existed,
                credentials: source,
            })
        }
        Err(e) => {
            if !existed {
                Settings::modify(&path, |s| remove_account(s, &def.id))?;
            }
            Err(e.into())
        }
    }
}

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

fn existing_folders(settings: &Settings, accounts: &[AccountDefinition]) -> Vec<ExistingFolder> {
    load_folders(settings, accounts)
        .folders
        .iter()
        .filter_map(|f| {
            let a = accounts.iter().find(|a| a.id == f.account_id)?;
            Some(ExistingFolder {
                path: folder_canonical_path(&f.definition.local_path),
                server: ServerIdentity {
                    url: a.url_string(),
                    user: a.credentials_user(),
                },
            })
        })
        .collect()
}

fn pick_account(
    accounts: &[AccountDefinition],
    id: Option<&str>,
) -> Result<AccountDefinition, ManageError> {
    match id {
        Some(id) => accounts
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| ManageError::Usage(format!("no account {id}"))),
        None => match accounts {
            [] => Err(ManageError::Usage(
                "no account configured: run `ncsync account add <server_url>` first".to_owned(),
            )),
            [one] => Ok(one.clone()),
            _ => Err(ManageError::Usage(
                "several accounts are configured: choose one with --account".to_owned(),
            )),
        },
    }
}

/// `folder add`: what the official client's "Add folder sync connection"
/// does (`AccountSettings::slotFolderWizardAccepted` and
/// `FolderMan::addFolder`): the local folder is created if needed, its path
/// is checked with `checkPathValidityForNewFolder`, the journal name is the
/// default one, the alias is the next free number, and the folder is saved
/// in `Folders` unless another account already uses the same path.
pub fn add_folder(
    ctx: &Context,
    local_dir: &str,
    remote_path: &str,
    account_id: Option<&str>,
) -> Result<FolderDefinition, ManageError> {
    let path = ctx.location.config_file.clone();
    let settings = ctx.load()?;
    let accounts = load_accounts(&settings).accounts;
    let account = pick_account(&accounts, account_id)?;
    let absolute =
        std::path::absolute(local_dir).map_err(|e| ManageError::Io(format!("{local_dir}: {e}")))?;
    let absolute = clean_path(&absolute.to_string_lossy());
    let existing = existing_folders(&settings, &accounts);
    let identity = ServerIdentity {
        url: account.url_string(),
        user: account.credentials_user(),
    };
    check_path_validity_for_new_folder(&absolute, Some(&identity), &existing)
        .map_err(|(_, msg)| ManageError::Usage(msg))?;
    if !Path::new(&absolute).exists() {
        log::info!("Creating folder {absolute}");
        std::fs::create_dir_all(&absolute).map_err(|e| {
            ManageError::Io(format!("Could not create local folder {absolute}: {e}"))
        })?;
    }
    let ((def, _group), _) = Settings::modify(&path, |s| {
        let accounts = load_accounts(s).accounts;
        let folders = load_folders(s, &accounts).folders;
        let mut def = FolderDefinition {
            local_path: FolderDefinition::prepare_local_path(&absolute),
            target_path: FolderDefinition::prepare_target_path(remote_path),
            // FolderMan::ignoreHiddenFiles(): the first folder's setting,
            // false when there is none.
            ignore_hidden_files: folders
                .first()
                .is_some_and(|f| f.definition.ignore_hidden_files),
            ..Default::default()
        };
        def.alias = allocate_alias("", &all_aliases(s), &[]);
        def.journal_path = def.default_journal_path(&account);
        let clean = clean_path(&folder_canonical_path(&def.local_path));
        let one_account_only = !folders
            .iter()
            .any(|f| clean_path(&folder_canonical_path(&f.definition.local_path)) == clean);
        let group = choose_group(false, one_account_only, one_account_only);
        save_folder(s, &account.id, group, &def);
        (def, group)
    })?;
    Ok(def)
}

/// `folder remove`: like `FolderMan::removeFolder`, the folder leaves the
/// configuration and its journal (with `-wal`/`-shm`) is deleted. A folder
/// taken over from the official client is refused (hand it back instead,
/// or `force`; its journal is then kept).
pub fn remove_folder(
    ctx: &Context,
    alias: &str,
    force: bool,
) -> Result<FolderDefinition, ManageError> {
    let path = ctx.location.config_file.clone();
    let (res, _) = Settings::modify(&path, |s| {
        let accounts = load_accounts(s).accounts;
        let found = load_folders(s, &accounts)
            .folders
            .into_iter()
            .find(|f| f.definition.alias == alias);
        let Some(found) = found else {
            return Err(ManageError::Usage(format!("no folder {alias:?}")));
        };
        let taken_over = taken_over_aliases(s).iter().any(|a| a == alias);
        if taken_over && !force {
            return Err(ManageError::Usage(format!(
                "folder {alias:?} was taken over from the official client: use `ncsync handback {alias}` (or --force to drop it and keep its journal)"
            )));
        }
        crate::folder_definition::remove_folder(s, &found.account_id, &found.escaped_alias);
        if taken_over {
            s.remove(&crate::settings::join_key(
                takeover::TAKEOVER_GROUP,
                &crate::folder_definition::escape_alias(alias),
            ));
        }
        Ok((found.definition, taken_over))
    })?;
    let (def, taken_over) = res?;
    if !taken_over && Path::new(&def.local_path).is_dir() {
        let db = def.absolute_journal_path();
        for suffix in ["", "-wal", "-shm"] {
            let f = format!("{db}{suffix}");
            if Path::new(&f).exists() {
                match std::fs::remove_file(&f) {
                    Ok(()) => {
                        log::info!(target: "nextcloud.gui.folder", "wipe: Removed csync StateDB {f}")
                    }
                    Err(e) => {
                        log::warn!(target: "nextcloud.gui.folder", "Failed to remove existing csync StateDB {f}: {e}")
                    }
                }
            }
        }
    }
    Ok(def)
}

// ---------------------------------------------------------------------------
// Takeover
// ---------------------------------------------------------------------------

/// How the credentials of a taken-over account were obtained.
#[derive(Debug)]
pub enum TakeoverCredentials {
    /// Ours already had the account.
    Existing,
    /// Copied from the official client's keyring item.
    FromOfficialClient(CredentialSource),
    /// Not found: the caller must log in (Login Flow v2 or an app password)
    /// and call [`store_account_password`].
    Missing(Box<AccountDefinition>),
}

/// `takeover`: see [`takeover`]. The app password of a newly created
/// account is copied from the official client's QtKeychain item when
/// possible.
pub fn take_over(
    ctx: &Context,
    official_file: &Path,
    selector: &str,
    store: Option<&dyn SecretStore>,
) -> Result<(TakeoverOutcome, TakeoverCredentials), ManageError> {
    let out = takeover::take_over_files(official_file, &ctx.location.config_file, selector)?;
    if !out.created_account {
        return Ok((out, TakeoverCredentials::Existing));
    }
    let settings = ctx.load()?;
    let mut ours = crate::account_config::load_account(&settings, &out.account_id)
        .ok_or_else(|| ManageError::Usage(format!("account {} vanished", out.account_id)))?;
    let secret = store.and_then(
        |s| match official_client_app_password(s, &out.official_account) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("cannot read the official client's keyring: {e}");
                None
            }
        },
    );
    let Some(secret) = secret else {
        return Ok((out, TakeoverCredentials::Missing(Box::new(ours))));
    };
    let source = store_account_password(ctx, &mut ours, &secret, store)?;
    Ok((out, TakeoverCredentials::FromOfficialClient(source)))
}

/// Stores the app password of a configured account (and saves its
/// `ncsyncd_passwordFile` key for a system instance).
pub fn store_account_password(
    ctx: &Context,
    account: &mut AccountDefinition,
    secret: &AppPassword,
    store: Option<&dyn SecretStore>,
) -> Result<CredentialSource, ManageError> {
    let resolver = CredentialResolver::new(ctx.location.mode.clone(), store);
    let source = resolver.store_app_password(account, secret, &ctx.location.state_dir)?;
    let def = account.clone();
    Settings::modify(&ctx.location.config_file, |s| save_account(s, &def))?;
    Ok(source)
}

/// `handback`: see [`takeover`]. `official_file` defaults to the one the
/// folder was taken from. Credentials of a removed account are forgotten.
pub fn hand_back(
    ctx: &Context,
    alias: &str,
    official_file: Option<&Path>,
    store: Option<&dyn SecretStore>,
) -> Result<HandbackOutcome, ManageError> {
    let settings = ctx.load()?;
    let source: PathBuf = match official_file {
        Some(p) => p.to_owned(),
        None => handback_source(&settings, alias)
            .ok_or_else(|| ManageError::Takeover(TakeoverError::NoBackup(alias.to_owned())))?,
    };
    let out = takeover::hand_back_files(&ctx.location.config_file, &source, alias)?;
    if let Some(acc) = &out.removed_account {
        let resolver = CredentialResolver::new(ctx.location.mode.clone(), store);
        if let Err(e) = resolver.forget(acc) {
            log::warn!(
                "could not delete the credentials of account {}: {e}",
                acc.id
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> Context {
        Context {
            location: ConfigLocation {
                mode: ServiceMode::User,
                config_file: dir.join("ncsyncd.cfg"),
                state_dir: dir.join("state"),
            },
            http: HttpClientOptions::default(),
        }
    }

    #[test]
    fn derived_folder_add_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        assert!(matches!(
            add_folder(&c, "/x", "/", None),
            Err(ManageError::Usage(m)) if m.contains("account add")
        ));
        Settings::modify(&c.location.config_file, |s| {
            save_account(
                s,
                &AccountDefinition::new_webflow("0", "https://h.example", "u"),
            );
        })
        .unwrap();
        let local = dir.path().join("sync/A");
        let def = add_folder(&c, local.to_str().unwrap(), "Docs/", None).unwrap();
        assert!(local.is_dir());
        assert_eq!(def.alias, "1");
        assert_eq!(def.target_path, "/Docs");
        assert!(def.journal_path.starts_with(".sync_"));
        let s = c.load().unwrap();
        let list = list_folders(&s);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].group, "Folders");
        assert!(!list[0].taken_over);
        assert_eq!(list_accounts(&s)[0].user, "u");
        // The same folder again is refused.
        assert!(add_folder(&c, local.to_str().unwrap(), "/", None).is_err());
        // A journal is removed with the folder.
        std::fs::write(local.join(&def.journal_path), b"").unwrap();
        remove_folder(&c, "1", false).unwrap();
        assert!(!local.join(&def.journal_path).exists());
        assert!(list_folders(&c.load().unwrap()).is_empty());
        assert!(remove_folder(&c, "1", false).is_err());
    }
}
