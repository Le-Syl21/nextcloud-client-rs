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
}

impl FileSettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
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

/// Loads the configuration into a new folder manager's accounts and
/// folders. Returns the resolver's view of which accounts have credentials.
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
    let store = match options.location.mode {
        ServiceMode::User => KeyringStore::secret_service()
            .map_err(|e| log::warn!(target: LOG, "The keyring is not available: {e}"))
            .ok(),
        ServiceMode::System { .. } => None,
    };
    let resolver = CredentialResolver::new(
        options.location.mode.clone(),
        store.as_ref().map(|s| s as &dyn SecretStore),
    );
    for acc in &accounts {
        let url = match ServerUrl::parse(&acc.url) {
            Ok(u) => u,
            Err(e) => {
                log::error!(target: LOG, "Account {}: invalid URL {:?}: {}", acc.id, acc.url, e.0);
                continue;
            }
        };
        let http = http_options(options, acc);
        let transport = match HttpTransport::new(&http) {
            Ok(t) => t,
            Err(e) => {
                log::error!(target: LOG, "Account {}: cannot set up the HTTP client: {e}", acc.id);
                continue;
            }
        };
        let account = Arc::new(Account::new(url, Arc::new(transport)));
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
        let push = (!options.no_push).then_some(http);
        fm.add_account(&acc.id, account, ready, limits_of(acc), push);
    }
    let official = official_client_paths(options);
    let loaded = load_folders(&settings, &accounts);
    for folder in loaded.folders {
        let def = folder.definition;
        if let Err(e) = def.check_supported() {
            log::error!(target: LOG, "{e}");
            continue;
        }
        if official.contains(&clean_path(&def.local_path)) {
            log::error!(target: LOG, "Folder {} ({}) is also configured in the official desktop client: not syncing it. Run `ncsync takeover {}` (with the official client stopped) to move it to ncsyncd, or remove it from one of the two clients.", def.alias, def.local_path, def.local_path);
            continue;
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
        let definition = Definition {
            alias: def.alias.clone(),
            local_path: def.local_path.clone(),
            journal_path: def.journal_path.clone(),
            target_path: def.target_path.clone(),
            paused: def.paused,
            ignore_hidden_files: def.ignore_hidden_files,
        };
        let Some(alias) = fm.add_folder_from_settings(
            &folder.account_id,
            definition,
            folder.save_backwards_compatible,
        ) else {
            log::error!(target: LOG, "Folder {}: its account {} is not available", def.alias, folder.account_id);
            continue;
        };
        if needs_save {
            let group = choose_group(false, folder.save_backwards_compatible, false);
            let account_id = folder.account_id.clone();
            let _ = Settings::modify(&config_path, |s| {
                crate::folder_definition::remove_folder(s, &account_id, &folder.escaped_alias);
                save_folder(s, &account_id, group, &def);
            });
        }
        log::info!(target: LOG, "Folder {alias}: {} <-> {}", def.local_path, def.target_path);
    }
    Ok(())
}

/// SIGHUP: re-reads the credentials of every account.
pub struct ReloadHooks {
    pub options: Options,
    pub accounts: Vec<(String, Arc<Account>)>,
}

impl crate::daemon::DaemonHooks for ReloadHooks {
    fn reload_credentials(&mut self) -> Vec<(String, bool)> {
        let Ok(settings) = Settings::load(&self.options.location.config_file) else {
            return Vec::new();
        };
        let accounts = load_accounts(&settings).accounts;
        let store = match self.options.location.mode {
            ServiceMode::User => KeyringStore::secret_service().ok(),
            ServiceMode::System { .. } => None,
        };
        let resolver = CredentialResolver::new(
            self.options.location.mode.clone(),
            store.as_ref().map(|s| s as &dyn SecretStore),
        );
        let mut out = Vec::new();
        for (id, account) in &self.accounts {
            let Some(acc) = accounts.iter().find(|a| &a.id == id) else {
                continue;
            };
            match resolver.app_password(acc) {
                Ok((password, source)) => {
                    log::info!(target: LOG, "Account {id}: credentials reloaded from {source}");
                    account.set_credentials(Credentials::new(
                        acc.credentials_user(),
                        password.expose(),
                    ));
                    out.push((id.clone(), true));
                }
                Err(e) => log::warn!(target: LOG, "Account {id}: {e}"),
            }
        }
        out
    }
}
