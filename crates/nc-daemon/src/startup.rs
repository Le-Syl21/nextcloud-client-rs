// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `ncsyncd` start-up: what `Application` does before the event loop runs
//! (`AccountManager::restore`, `FolderMan::setupFolders`), from the
//! configuration file, plus the guard against syncing a folder the official
//! client also syncs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nc_dav::{Account, Credentials, HttpClientOptions, HttpTransport, ServerUrl};

use crate::account_config::{AccountDefinition, load_accounts};
use crate::config_file::{ConfigFile, ConfigLocation, ServiceMode, official_client_config_file};
use crate::credentials::{CredentialResolver, KeyringStore, SecretStore};
use crate::folder::{Definition, FolderSettings};
use crate::folder_definition::{
    FolderDefinition, JournalMigration, choose_group, clean_path, escape_alias, load_folders,
    move_absolute_journal, save_folder,
};
use crate::folder_man::{FolderMan, FolderManSettings, FolderSettingsStore};
use crate::network_limits::{LimitSetting, NetworkLimits};
use crate::settings::{Settings, SettingsError};

const LOG: &str = "nextcloud.daemon";

/// Start-up options.
#[derive(Clone, Debug)]
pub struct Options {
    pub location: ConfigLocation,
    /// Accept invalid TLS certificates for every account.
    pub trust: bool,
    /// An explicit HTTP proxy for every account.
    pub http_proxy: Option<String>,
    /// Do not watch the folders with inotify (full local discoveries only).
    pub no_watch: bool,
    /// Do not connect to notify_push (etag polling only).
    pub no_push: bool,
    /// The official client's configuration (default: its standard place).
    pub official_config: Option<PathBuf>,
}

/// Errors that prevent the daemon from starting.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("cannot read the configuration {path}: {source} (create it with `ncsync account add`)")]
    Config {
        path: PathBuf,
        source: SettingsError,
    },
    #[error("the configuration {0} has no account (add one with `ncsync account add`)")]
    NoAccount(PathBuf),
}

/// The folder settings store of the daemon: the configuration file,
/// re-read and written under its lock at every change.
pub struct FileSettingsStore {
    path: PathBuf,
    /// Where the credentials of a removed account are forgotten.
    mode: ServiceMode,
}

impl FileSettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: ServiceMode::User,
        }
    }

    /// The service mode of the configuration (the keyring is used for the
    /// user daemon only).
    pub fn with_mode(mut self, mode: ServiceMode) -> Self {
        self.mode = mode;
        self
    }
}

fn to_folder_definition(def: &Definition) -> FolderDefinition {
    FolderDefinition {
        alias: def.alias.clone(),
        local_path: def.local_path.clone(),
        journal_path: def.journal_path.clone(),
        target_path: def.target_path.clone(),
        paused: def.paused,
        ignore_hidden_files: def.ignore_hidden_files,
        ..FolderDefinition::default()
    }
}

impl FolderSettingsStore for FileSettingsStore {
    fn save_folder(
        &mut self,
        account_id: &str,
        definition: &Definition,
        backwards_compatible: bool,
    ) {
        let group = choose_group(false, backwards_compatible, false);
        let def = to_folder_definition(definition);
        if let Err(e) = Settings::modify(&self.path, |s| save_folder(s, account_id, group, &def)) {
            log::warn!(target: LOG, "Could not save folder {} to {}: {e}", definition.alias, self.path.display());
        }
    }

    fn remove_folder(&mut self, account_id: &str, alias: &str) {
        let escaped = escape_alias(alias);
        if let Err(e) = Settings::modify(&self.path, |s| {
            crate::folder_definition::remove_folder(s, account_id, &escaped)
        }) {
            log::warn!(target: LOG, "Could not remove folder {alias} from {}: {e}", self.path.display());
        }
    }

    fn remove_account(&mut self, account_id: &str) {
        let removed = Settings::modify(&self.path, |s| {
            let acc = crate::account_config::load_account(s, account_id);
            crate::account_config::remove_account(s, account_id);
            acc
        });
        let acc = match removed {
            Ok((acc, _)) => acc,
            Err(e) => {
                log::warn!(target: LOG, "Could not remove account {account_id} from {}: {e}", self.path.display());
                return;
            }
        };
        let Some(acc) = acc else {
            return;
        };
        let store = match self.mode {
            ServiceMode::User => KeyringStore::secret_service().ok(),
            ServiceMode::System { .. } => None,
        };
        let resolver = CredentialResolver::new(
            self.mode.clone(),
            store.as_ref().map(|s| s as &dyn SecretStore),
        );
        if let Err(e) = resolver.forget(&acc) {
            log::warn!(target: LOG, "Could not delete the credentials of account {account_id}: {e}");
        }
    }
}

/// The `ConfigFile` values of the folder manager and the folders.
pub fn folder_man_settings(cfg: &ConfigFile<'_>, exclude_files: Vec<String>) -> FolderManSettings {
    FolderManSettings {
        remote_poll_interval: cfg.remote_poll_interval(),
        force_sync_interval: cfg.force_sync_interval(),
        folder: FolderSettings {
            new_big_folder_size_limit: cfg.new_big_folder_size_limit(),
            confirm_external_storage: cfg.confirm_external_storage(),
            move_to_trash: cfg.move_to_trash(),
            min_chunk_size: cfg.min_chunk_size(),
            max_chunk_size: cfg.max_chunk_size(),
            chunk_size: cfg.chunk_size(),
            full_local_discovery_interval: cfg.full_local_discovery_interval_with_env(),
            stop_syncing_existing_folders_over_limit: cfg
                .stop_syncing_existing_folders_over_limit(),
            notify_existing_folders_over_limit: cfg.notify_existing_folders_over_limit(),
            prompt_delete_files: cfg.prompt_delete_files(),
            delete_files_threshold: cfg.delete_files_threshold(),
            exclude_files,
            ..FolderSettings::default()
        },
    }
}

/// `ConfigFile::setupDefaultExcludeFilePaths`: the system list
/// (`/etc/Nextcloud/sync-exclude.lst`, or the bundled default list written
/// to the state directory) and the user list next to the configuration
/// (`<config dir>/sync-exclude.lst`) when it exists.
pub fn exclude_files(location: &ConfigLocation) -> Vec<String> {
    let mut out = Vec::new();
    let system = Path::new("/etc/Nextcloud/sync-exclude.lst");
    if system.exists() {
        out.push(system.to_string_lossy().into_owned());
    } else {
        let path = location.state_dir.join("sync-exclude.lst");
        let up_to_date = std::fs::read(&path)
            .is_ok_and(|c| c == nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST.as_bytes());
        if !up_to_date {
            let _ = std::fs::create_dir_all(&location.state_dir);
            if let Err(e) = std::fs::write(&path, nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST) {
                log::warn!(target: LOG, "Could not write the default exclude list to {}: {e}", path.display());
            }
        }
        out.push(path.to_string_lossy().into_owned());
    }
    if let Some(dir) = location.config_file.parent() {
        let user = dir.join("sync-exclude.lst");
        if user.exists() {
            out.push(user.to_string_lossy().into_owned());
        }
    }
    out
}

fn limits_of(acc: &AccountDefinition) -> NetworkLimits {
    let mut l = NetworkLimits::default();
    l.set_upload_limit_setting(LimitSetting::from_int(acc.upload_limit_setting.to_int()));
    l.set_download_limit_setting(LimitSetting::from_int(acc.download_limit_setting.to_int()));
    l.set_upload_limit(acc.upload_limit.clamp(0, i64::from(u32::MAX)) as u32);
    l.set_download_limit(acc.download_limit.clamp(0, i64::from(u32::MAX)) as u32);
    l
}

fn http_options(options: &Options, acc: &AccountDefinition) -> HttpClientOptions {
    // Per-account proxy settings (QNetworkProxy types 3/4: HTTP).
    let proxy = options.http_proxy.clone().or_else(|| {
        let p = &acc.proxy;
        (matches!(p.proxy_type, 3 | 4) && !p.host.is_empty())
            .then(|| format!("http://{}:{}", p.host, p.port))
    });
    HttpClientOptions {
        trust_invalid_certificates: options.trust,
        proxy,
    }
}

/// The local paths the official client's configuration syncs (empty when
/// it has none or cannot be read).
fn official_client_paths(options: &Options) -> Vec<String> {
    let Some(path) = options
        .official_config
        .clone()
        .or_else(|| official_client_config_file().ok())
    else {
        return Vec::new();
    };
    if !path.exists() {
        return Vec::new();
    }
    let Ok(settings) = Settings::load(&path) else {
        log::warn!(target: LOG, "Could not read the official client's configuration {}", path.display());
        return Vec::new();
    };
    let accounts = load_accounts(&settings).accounts;
    load_folders(&settings, &accounts)
        .folders
        .into_iter()
        .map(|f| clean_path(&f.definition.local_path))
        .collect()
}

/// The keyring of the user daemon (`None` for a system instance or when
/// there is none).
fn credential_store(options: &Options) -> Option<KeyringStore> {
    match options.location.mode {
        ServiceMode::User => KeyringStore::secret_service()
            .map_err(|e| log::warn!(target: LOG, "The keyring is not available: {e}"))
            .ok(),
        ServiceMode::System { .. } => None,
    }
}

/// The `Account` of a configured account, with its credentials when they
/// are found (`AccountManager::loadAccountHelper`). `None` when its URL or
/// HTTP client is unusable.
fn build_account(
    options: &Options,
    acc: &AccountDefinition,
    resolver: &CredentialResolver<'_>,
) -> Option<(Arc<Account>, bool)> {
    let url = match ServerUrl::parse(&acc.url) {
        Ok(u) => u,
        Err(e) => {
            log::error!(target: LOG, "Account {}: invalid URL {:?}: {}", acc.id, acc.url, e.0);
            return None;
        }
    };
    let http = http_options(options, acc);
    let transport = match HttpTransport::new(&http) {
        Ok(t) => t,
        Err(e) => {
            log::error!(target: LOG, "Account {}: cannot set up the HTTP client: {e}", acc.id);
            return None;
        }
    };
    let account = Arc::new(Account::new(url, Arc::new(transport)));
    // ClientStatusReporting: its databases next to the configuration
    // (`ConfigFile().configPath()`).
    if let Some(dir) = options.location.config_file.parent() {
        nc_sync::client_status_reporting::install(&account, dir.to_owned());
    }
    let user = acc.credentials_user();
    let ready = match resolver.app_password(acc) {
        Ok((password, source)) => {
            log::info!(target: LOG, "Account {}: credentials from {source}", acc.id);
            account.set_credentials(Credentials::new(user.clone(), password.expose()));
            true
        }
        Err(e) => {
            log::warn!(target: LOG, "Account {}: {e}", acc.id);
            account.set_credentials(Credentials::new(user.clone(), String::new()));
            false
        }
    };
    if !acc.dav_user.is_empty() {
        account.set_dav_user(&acc.dav_user);
    }
    if !acc.display_name.is_empty() {
        account.set_dav_display_name(&acc.display_name);
    }
    if !acc.server_version.is_empty() {
        account.set_server_version(&acc.server_version);
    }
    Some((account, ready))
}

fn add_account(
    fm: &mut FolderMan,
    options: &Options,
    acc: &AccountDefinition,
    resolver: &CredentialResolver<'_>,
) -> bool {
    let Some((account, ready)) = build_account(options, acc, resolver) else {
        return false;
    };
    let push = (!options.no_push).then(|| http_options(options, acc));
    fm.add_account(&acc.id, account, ready, limits_of(acc), push);
    true
}

/// A folder of the configuration, ready to be added to the folder manager.
struct WantedFolder {
    account_id: String,
    definition: Definition,
    backwards_compatible: bool,
}

/// The identity of a folder: what makes a loaded folder the same as a
/// configured one (other changes need a restart).
fn same_folder(f: &crate::folder::Folder, account_id: &str, def: &FolderDefinition) -> bool {
    f.alias() == def.alias
        && f.account_id() == account_id
        && clean_path(f.path()) == clean_path(&def.local_path)
        && f.remote_path() == crate::folder::prepare_target_path(&def.target_path)
}

/// The checks and migrations of `FolderMan::setupFoldersHelper` for one
/// loaded folder definition: virtual files are refused, so is a folder the
/// official client also syncs; a journal is moved from an absolute path,
/// and a changed definition is saved.
fn prepare_folder(
    options: &Options,
    accounts: &[AccountDefinition],
    official: &[String],
    folder: crate::folder_definition::LoadedFolder,
) -> Option<WantedFolder> {
    let config_path = &options.location.config_file;
    let def = folder.definition;
    if let Err(e) = def.check_supported() {
        log::error!(target: LOG, "{e}");
        return None;
    }
    if official.contains(&clean_path(&def.local_path)) {
        log::error!(target: LOG, "Folder {} ({}) is also configured in the official desktop client: not syncing it. Run `ncsync takeover {}` (with the official client stopped) to move it to ncsyncd, or remove it from one of the two clients.", def.alias, def.local_path, def.local_path);
        return None;
    }
    let mut def = def;
    let mut needs_save = folder.needs_save;
    match &folder.journal_migration {
        JournalMigration::FromAbsolutePath(old) => {
            if !move_absolute_journal(old, &def) {
                log::warn!(target: LOG, "Wasn't able to move 3.0 syncjournal database files to new location. One-time loss off sync settings possible.");
            }
            needs_save = true;
        }
        JournalMigration::MaybeMigrateLegacyDb => {
            let _ = nc_journal::journal::maybe_migrate_db(
                &def.local_path,
                &def.absolute_journal_path(),
            );
        }
        JournalMigration::None => {}
    }
    if def.journal_path.is_empty()
        && let Some(acc) = accounts.iter().find(|a| a.id == folder.account_id)
    {
        def.journal_path = def.default_journal_path(acc);
    }
    if needs_save {
        let group = choose_group(false, folder.save_backwards_compatible, false);
        let account_id = folder.account_id.clone();
        let _ = Settings::modify(config_path, |s| {
            crate::folder_definition::remove_folder(s, &account_id, &folder.escaped_alias);
            save_folder(s, &account_id, group, &def);
        });
    }
    Some(WantedFolder {
        account_id: folder.account_id,
        definition: Definition {
            alias: def.alias.clone(),
            local_path: def.local_path.clone(),
            journal_path: def.journal_path.clone(),
            target_path: def.target_path.clone(),
            paused: def.paused,
            ignore_hidden_files: def.ignore_hidden_files,
        },
        backwards_compatible: folder.save_backwards_compatible,
    })
}

/// `addFolderInternal` of a prepared folder, with its log line.
fn add_folder(fm: &mut FolderMan, wanted: WantedFolder) -> Option<String> {
    let (local, remote) = (
        wanted.definition.local_path.clone(),
        wanted.definition.target_path.clone(),
    );
    let name = wanted.definition.alias.clone();
    let Some(alias) = fm.add_folder_from_settings(
        &wanted.account_id,
        wanted.definition,
        wanted.backwards_compatible,
    ) else {
        log::error!(target: LOG, "Folder {name}: its account {} is not available", wanted.account_id);
        return None;
    };
    log::info!(target: LOG, "Folder {alias}: {local} <-> {remote}");
    Some(alias)
}

/// Loads the configuration into a new folder manager's accounts and
/// folders.
pub fn setup(fm: &mut FolderMan, options: &Options) -> Result<(), StartupError> {
    let config_path = options.location.config_file.clone();
    let settings = Settings::load(&config_path).map_err(|source| StartupError::Config {
        path: config_path.clone(),
        source,
    })?;
    let accounts = load_accounts(&settings).accounts;
    if accounts.is_empty() {
        return Err(StartupError::NoAccount(config_path));
    }
    let store = credential_store(options);
    let resolver = CredentialResolver::new(
        options.location.mode.clone(),
        store.as_ref().map(|s| s as &dyn SecretStore),
    );
    for acc in &accounts {
        add_account(fm, options, acc, &resolver);
    }
    let official = official_client_paths(options);
    for folder in load_folders(&settings, &accounts).folders {
        if let Some(wanted) = prepare_folder(options, &accounts, &official, folder) {
            add_folder(fm, wanted);
        }
    }
    Ok(())
}

/// What a [`reload`] changed (aliases and account ids).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReloadOutcome {
    pub added_accounts: Vec<String>,
    pub removed_accounts: Vec<String>,
    /// Accounts whose credentials changed or were found.
    pub updated_credentials: Vec<String>,
    pub added_folders: Vec<String>,
    pub removed_folders: Vec<String>,
    /// The removed folders whose journal is wiped (now, or once their
    /// running sync stopped).
    pub wiped_folders: Vec<String>,
    /// The removed folders whose sync is still stopping.
    #[serde(skip)]
    pub waiting: Vec<crate::event::FolderId>,
}

/// Re-reads the configuration of a running daemon (`ncsync` after it changed
/// it, SIGHUP), the way the official client's `FolderMan` adds and removes
/// folders at runtime:
///
/// 1. accounts that left the configuration (or changed server or login
///    name) are removed with their folders (`removeAccountState`, the
///    stored credentials are left alone);
/// 2. loaded folders no longer configured (or refused now) are removed:
///    `removeFolder` (journal wiped) for the aliases in `wipe`,
///    `unloadFolder` (journal closed and kept) for the others; a running
///    sync is aborted first, and the journal is only touched once its
///    engine is back ([`ReloadOutcome::waiting`]);
/// 3. new accounts are added, and the credentials of the others re-read
///    (an account that found new credentials connects again);
/// 4. new folders are added and scheduled (`addFolderInternal`).
///
/// Global settings (poll intervals, chunk sizes, ...) and changed account
/// options (proxy) need a restart.
pub fn reload(
    fm: &mut FolderMan,
    options: &Options,
    wipe: &[String],
) -> Result<ReloadOutcome, StartupError> {
    let config_path = options.location.config_file.clone();
    log::info!(target: LOG, "Reloading the configuration {}", config_path.display());
    let settings = Settings::load(&config_path).map_err(|source| StartupError::Config {
        path: config_path.clone(),
        source,
    })?;
    let accounts = load_accounts(&settings).accounts;
    let official = official_client_paths(options);
    let loaded = load_folders(&settings, &accounts).folders;
    let mut out = ReloadOutcome::default();

    // 1. accounts gone (or no longer the same server/user)
    let same_account = |state: &crate::account_state::AccountState, acc: &AccountDefinition| {
        let url = ServerUrl::parse(&acc.url).map(|u| u.to_credential_free_string());
        url.is_ok_and(|u| u == state.account().url().to_credential_free_string())
            && state.account().credentials().user == acc.credentials_user()
    };
    let gone: Vec<String> = fm
        .account_states()
        .filter(|state| {
            !accounts
                .iter()
                .any(|a| a.id == state.id() && same_account(state, a))
        })
        .map(|state| state.id().to_owned())
        .collect();

    // 2. folders gone: those of the configuration that would be refused or
    // belong to a gone account count as gone too.
    let wanted_ids: Vec<(String, FolderDefinition)> = loaded
        .iter()
        .filter(|f| !gone.contains(&f.account_id))
        .filter(|f| f.definition.check_supported().is_ok())
        .filter(|f| !official.contains(&clean_path(&f.definition.local_path)))
        .map(|f| (f.account_id.clone(), f.definition.clone()))
        .collect();
    let removed: Vec<String> = fm
        .map()
        .values()
        .filter(|f| {
            !wanted_ids
                .iter()
                .any(|(account_id, def)| same_folder(f, account_id, def))
        })
        .map(|f| f.alias().to_owned())
        .collect();
    for alias in removed {
        let wiped = wipe.contains(&alias);
        if let Some(id) = fm.unload_folder(&alias, wiped) {
            out.waiting.push(id);
        }
        if wiped {
            out.wiped_folders.push(alias.clone());
        }
        out.removed_folders.push(alias);
    }
    for id in gone {
        out.waiting.extend(fm.remove_account_state(&id));
        out.removed_accounts.push(id);
    }

    // 3. new accounts, new credentials
    let store = credential_store(options);
    let resolver = CredentialResolver::new(
        options.location.mode.clone(),
        store.as_ref().map(|s| s as &dyn SecretStore),
    );
    for acc in &accounts {
        let Some(state) = fm.account_state(&acc.id) else {
            if add_account(fm, options, acc, &resolver) {
                out.added_accounts.push(acc.id.clone());
            }
            continue;
        };
        let needs_credentials = matches!(
            state.state(),
            crate::account_state::State::SignedOut | crate::account_state::State::AskingCredentials
        );
        let current = state.account().credentials().password;
        match resolver.app_password(acc) {
            Ok((password, source)) => {
                if needs_credentials || password.expose() != current {
                    log::info!(target: LOG, "Account {}: credentials reloaded from {source}", acc.id);
                    fm.set_account_credentials(
                        &acc.id,
                        Credentials::new(acc.credentials_user(), password.expose()),
                    );
                    out.updated_credentials.push(acc.id.clone());
                }
            }
            Err(e) => log::warn!(target: LOG, "Account {}: {e}", acc.id),
        }
        fm.set_network_limits(&acc.id, limits_of(acc));
    }

    // 4. new folders
    for folder in loaded {
        let loaded_already = fm
            .map()
            .values()
            .any(|f| same_folder(f, &folder.account_id, &folder.definition));
        if loaded_already {
            continue;
        }
        if let Some(wanted) = prepare_folder(options, &accounts, &official, folder)
            && let Some(alias) = add_folder(fm, wanted)
        {
            out.added_folders.push(alias);
        }
    }
    log::info!(target: LOG, "Configuration reloaded: {} accounts, {} folders", fm.account_states().count(), fm.map().len());
    Ok(out)
}

/// The daemon's [`crate::daemon::DaemonHooks`]: [`reload`] for SIGHUP and the
/// `reload` control request.
pub struct ReloadHooks {
    pub options: Options,
}

impl crate::daemon::DaemonHooks for ReloadHooks {
    fn reload(
        &mut self,
        fm: &mut FolderMan,
        config: Option<&str>,
        wipe: &[String],
    ) -> crate::daemon::ReloadResult {
        let ours = &self.options.location.config_file;
        if let Some(theirs) = config {
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
            if canon(Path::new(theirs)) != canon(ours) {
                return crate::daemon::ReloadResult::error(format!(
                    "this daemon uses the configuration {}, not {theirs}",
                    ours.display()
                ));
            }
        }
        match reload(fm, &self.options, wipe) {
            Ok(out) => {
                let mut response = serde_json::to_value(&out).unwrap_or_default();
                response["ok"] = serde_json::Value::Bool(true);
                response["config"] = serde_json::Value::from(ours.to_string_lossy().as_ref());
                crate::daemon::ReloadResult {
                    response,
                    waiting: out.waiting,
                }
            }
            Err(e) => {
                log::error!(target: LOG, "Could not reload the configuration: {e}");
                crate::daemon::ReloadResult::error(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_config::save_account;
    use crate::manage::{self, Context};

    fn account(id: &str, dir: &Path) -> AccountDefinition {
        let mut acc = AccountDefinition::new_webflow(id, "http://127.0.0.1:9", &format!("u{id}"));
        let pw = dir.join(format!("pw{id}"));
        std::fs::write(&pw, "secret\n").unwrap();
        acc.password_file = Some(pw.to_string_lossy().into_owned());
        acc
    }

    #[test]
    fn derived_reload_adds_and_removes_accounts_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let location = ConfigLocation {
            mode: ServiceMode::System {
                instance: "test".to_owned(),
            },
            config_file: dir.path().join("ncsyncd.cfg"),
            state_dir: dir.path().join("state"),
        };
        let options = Options {
            location: location.clone(),
            trust: false,
            http_proxy: None,
            no_watch: true,
            no_push: true,
            official_config: Some(dir.path().join("no-official.cfg")),
        };
        let ctx = Context {
            location: location.clone(),
            http: HttpClientOptions::default(),
        };
        let cfg = location.config_file.clone();
        Settings::modify(&cfg, |s| save_account(s, &account("0", dir.path()))).unwrap();
        let local = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let one = manage::add_folder(&ctx, &local("A"), "/", None).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut fm = FolderMan::new(
                &tx,
                FolderManSettings::default(),
                Box::new(crate::folder_man::NullSettingsStore),
                crate::folder_man::no_watcher(),
            );
            setup(&mut fm, &options).unwrap();
            assert_eq!(fm.map().len(), 1);

            // nothing changed
            assert_eq!(
                reload(&mut fm, &options, &[]).unwrap(),
                ReloadOutcome::default()
            );

            // a folder, an account and its folder
            let two = manage::add_folder(&ctx, &local("B"), "/Docs", None).unwrap();
            Settings::modify(&cfg, |s| save_account(s, &account("1", dir.path()))).unwrap();
            let three = manage::add_folder(&ctx, &local("C"), "/", Some("1")).unwrap();
            let out = reload(&mut fm, &options, &[]).unwrap();
            assert_eq!(out.added_accounts, ["1"]);
            assert_eq!(out.added_folders, [two.alias.clone(), three.alias.clone()]);
            assert!(out.removed_folders.is_empty());
            assert_eq!(fm.map().len(), 3);
            assert_eq!(fm.account_states().count(), 2);

            // folder 1 removed (journal wiped), account 1 removed with its
            // folder (journal kept)
            let journal = |def: &FolderDefinition| PathBuf::from(def.absolute_journal_path());
            std::fs::write(journal(&one), b"").unwrap();
            std::fs::write(journal(&three), b"").unwrap();
            manage::remove_folder(&ctx, &one.alias, false).unwrap();
            Settings::modify(&cfg, |s| {
                crate::folder_definition::remove_folder(
                    s,
                    "1",
                    &crate::folder_definition::escape_alias(&three.alias),
                );
                crate::account_config::remove_account(s, "1");
            })
            .unwrap();
            let out = reload(&mut fm, &options, std::slice::from_ref(&one.alias)).unwrap();
            assert_eq!(out.removed_accounts, ["1"]);
            assert_eq!(
                out.removed_folders,
                [one.alias.clone(), three.alias.clone()]
            );
            assert_eq!(out.wiped_folders, std::slice::from_ref(&one.alias));
            assert!(out.waiting.is_empty());
            assert!(!journal(&one).exists());
            assert!(journal(&three).exists());
            assert_eq!(
                fm.map().keys().cloned().collect::<Vec<_>>(),
                std::slice::from_ref(&two.alias)
            );
            assert_eq!(fm.account_states().count(), 1);

            // a folder the official client also syncs is unloaded
            let official = dir.path().join("official.cfg");
            std::fs::copy(&cfg, &official).unwrap();
            let with_official = Options {
                official_config: Some(official),
                ..options.clone()
            };
            let out = reload(&mut fm, &with_official, &[]).unwrap();
            assert_eq!(out.removed_folders, std::slice::from_ref(&two.alias));
            assert!(fm.map().is_empty());
        });
    }
}
