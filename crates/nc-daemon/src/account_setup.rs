// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/accountsetupfromcommandlinejob.cpp` and the
// setup part of `src/gui/accountsetupcommandlinemanager.cpp`
// (nextcloud/desktop v34.0.5), on top of the daemon's configuration.

//! `AccountSetupFromCommandLineJob`: the account provisioning of
//! `nextcloudcmd --userid ... --serverurl ... [--apppassword ...]`
//! (`ncsync sync --userid ...` here). The account and its folder are written
//! to the daemon's configuration, only once the credentials were checked
//! against the server, so a failed setup leaves nothing behind.
//!
//! Every path ends with one status message (`printAccountSetupFromCommandLineStatusAndExit`),
//! printed by the caller: on stdout for a success, on stderr for a failure.
//! A failure found before anything is sent to the server is
//! [`SetupStatus::rejected`] (`handleAccountSetupFromCommandLine()` returned
//! false: `nextcloudcmd` returns -1, exit code 255); the others end the
//! event loop with exit code 1 (failure) or 0 (success).
//!
//! Not ported: the File Provider mode (macOS), `Utility::setupFavLink` (the
//! GTK bookmark of the folder: desktop integration), virtual files (the
//! caller refuses `--isvfsenabled 1`), the keychain wait (our credential
//! stores are synchronous).

use std::path::Path;
use std::sync::Arc;

use nc_dav::{Account, Credentials, HttpTransport, JobOptions, NetworkError, ServerUrl};
use nc_journal::journal::{SelectiveSyncListType, SyncJournalDb};

use crate::account_config::{
    AccountDefinition, generate_free_account_id, load_accounts, remove_account, save_account,
};
use crate::config_file::ConfigFile;
use crate::credentials::{AppPassword, CredentialResolver, SecretStore};
use crate::folder_definition::{
    ExistingFolder, FolderDefinition, GoodPathStrategy, ServerIdentity, all_aliases,
    allocate_alias, choose_group, clean_path, find_good_path_for_new_sync_folder,
    folder_canonical_path, load_folders, save_folder,
};
use crate::manage::Context;
use crate::settings::Settings;

const LOG: &str = "nextcloud.gui.accountsetupcommandlinejob";

/// `Theme::defaultClientFolder()` (`appName()`).
const DEFAULT_CLIENT_FOLDER: &str = "Nextcloud";

/// What the command line gave (`AccountSetupCommandLineManager`'s fields).
#[derive(Clone, Debug, Default)]
pub struct SetupParams {
    pub app_password: String,
    pub user_id: String,
    /// The `--serverurl` value, as given.
    pub server_url: String,
    pub local_dir_path: String,
    pub remote_dir_path: String,
}

/// The end of the setup: the message of
/// `printAccountSetupFromCommandLineStatusAndExit(status, isFailure)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupStatus {
    pub message: String,
    pub is_failure: bool,
    /// The setup stopped before the event loop
    /// (`handleAccountSetupFromCommandLine()` returned false).
    pub rejected: bool,
}

impl SetupStatus {
    fn rejected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_failure: true,
            rejected: true,
        }
    }

    fn failure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_failure: true,
            rejected: false,
        }
    }

    fn success(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_failure: false,
            rejected: false,
        }
    }

    /// The process exit code: 255 for `return -1`, else the code the job
    /// exits the event loop with.
    pub fn exit_code(&self) -> u8 {
        if self.rejected {
            255
        } else if self.is_failure {
            1
        } else {
            0
        }
    }
}

/// `QUrl(value).isValid()` for the `--serverurl` value (`TolerantMode`): a
/// non-empty URL whose scheme, when there is one, is well formed and whose
/// authority has a valid host and port; without a scheme (a relative URL),
/// its first path segment must not contain a `:`.
pub fn qurl_is_valid(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let scheme_end = value.find(':').filter(|&i| {
        let scheme = &value[..i];
        scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let Some(scheme_end) = scheme_end else {
        // Relative URL: "Relative URL's path component contains ':' before any '/'".
        let first_segment = value.split(['/', '?', '#']).next().unwrap_or_default();
        return !first_segment.contains(':');
    };
    let rest = &value[scheme_end + 1..];
    let Some(after) = rest.strip_prefix("//") else {
        return true;
    };
    let authority = after.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
        match v6.split_once(']') {
            Some((addr, port)) => {
                if !addr
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
                {
                    return false;
                }
                ("", port.strip_prefix(':'))
            }
            None => return false,
        }
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    let host_ok = host.chars().all(|c| {
        c.is_alphanumeric()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
            )
    });
    let port_ok = port.is_none_or(|p| p.is_empty() || p.parse::<u16>().is_ok());
    host_ok && port_ok
}

/// `Utility::escape` (`QString::toHtmlEscaped`).
fn html_escaped(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `Account::displayName()`: `user@host`, with the port unless 80 or 443.
pub fn display_name(user: &str, url: &ServerUrl) -> String {
    let mut name = format!("{user}@{}", url.host);
    if let Some((_, port)) = url.authority.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
        && port > 0
        && port != 80
        && port != 443
    {
        name.push_str(&format!(":{port}"));
    }
    name
}

/// The host of a configured account URL (lower case, like `QUrl::host()`).
fn host_of(url: &str) -> String {
    ServerUrl::parse(url)
        .map(|u| u.host.to_lowercase())
        .unwrap_or_default()
}

/// The configured folders, for the path checks.
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

/// `QDir(path).isEmpty()`: no entry besides `.` and `..` (hidden ones count).
fn dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path).map_or(true, |mut d| d.next().is_none())
}

/// The account setup job.
pub struct AccountSetupFromCommandLineJob<'a> {
    ctx: &'a Context,
    store: Option<&'a dyn SecretStore>,
    params: SetupParams,
}

impl<'a> AccountSetupFromCommandLineJob<'a> {
    pub fn new(ctx: &'a Context, store: Option<&'a dyn SecretStore>, params: SetupParams) -> Self {
        Self { ctx, store, params }
    }

    /// `defaultLocalDirPath()`: `overrideLocalDir`, else `~/Nextcloud`,
    /// made unique with `findGoodPathForNewSyncFolder`.
    fn default_local_dir_path(&self, settings: &Settings, url: &ServerUrl) -> String {
        let override_local_dir = ConfigFile::new(settings).override_local_dir();
        let mut local_dir_path = override_local_dir.clone();
        if local_dir_path.is_empty() {
            local_dir_path = DEFAULT_CLIENT_FOLDER.to_owned();
            if !Path::new(&local_dir_path).is_absolute() {
                let home = std::env::var("HOME").unwrap_or_default();
                local_dir_path = format!("{home}/{local_dir_path}");
            }
        }
        let accounts = load_accounts(settings).accounts;
        let existing = existing_folders(settings, &accounts);
        let identity = ServerIdentity {
            url: url.to_credential_free_string(),
            user: self.params.user_id.clone(),
        };
        find_good_path_for_new_sync_folder(
            &local_dir_path,
            Some(&identity),
            &existing,
            if override_local_dir.is_empty() {
                GoodPathStrategy::AllowOnlyNewPath
            } else {
                GoodPathStrategy::AllowOverrideExistingPath
            },
        )
    }

    /// `handleAccountSetupFromCommandLine()` and the asynchronous rest of
    /// the job, up to `printAccountSetupFromCommandLineStatusAndExit`.
    pub async fn run(mut self) -> SetupStatus {
        let settings = match self.ctx.load() {
            Ok(s) => s,
            Err(e) => {
                return SetupStatus::rejected(format!("Could not read the configuration: {e}"));
            }
        };
        // `_serverUrl.host()` of the given URL.
        let given = self.params.server_url.trim().trim_end_matches(['/', '\\']);
        let parsed = (given.starts_with("http://") || given.starts_with("https://"))
            .then(|| ServerUrl::parse(given).ok())
            .flatten();
        let host = parsed
            .as_ref()
            .map(|u| u.host.to_lowercase())
            .unwrap_or_default();
        let user_id_at_host = format!("{}@{host}", self.params.user_id);
        let accounts = load_accounts(&settings).accounts;
        if accounts
            .iter()
            .any(|a| format!("{}@{}", a.effective_dav_user(), host_of(&a.url)) == user_id_at_host)
        {
            return SetupStatus::rejected(format!(
                "Account {} already exists!",
                self.params.user_id
            ));
        }

        // An URL QUrl accepts but no request can be sent to (no http/https
        // scheme): upstream's requests fail with "Protocol is unknown".
        let Some(url) = parsed else {
            if self.params.app_password.is_empty() {
                return SetupStatus::failure(format!(
                    "Account {} setup from command line failed with error: unsupported server URL {}.",
                    self.params.user_id, self.params.server_url
                ));
            }
            return SetupStatus::failure("Could not fetch username.");
        };
        let url = ServerUrl {
            user: String::new(),
            password: String::new(),
            ..url
        };

        if self.params.local_dir_path.is_empty() {
            // The local folder is documented as optional, so fall back to the folder the
            // account wizard would suggest rather than refusing to set the account up.
            self.params.local_dir_path = self.default_local_dir_path(&settings, &url);
        }
        if self.params.local_dir_path.is_empty() {
            return SetupStatus::rejected(
                "Could not determine a local folder to sync into. Please pass one with --localdirpath.",
            );
        }
        let local_dir = Path::new(&self.params.local_dir_path);
        if local_dir.is_dir() && !dir_is_empty(local_dir) {
            return SetupStatus::rejected(format!(
                "Local folder {} already exists and is non-empty!",
                self.params.local_dir_path
            ));
        }

        let transport = match HttpTransport::new(&self.ctx.http) {
            Ok(t) => t,
            Err(e) => {
                return SetupStatus::failure(format!("Could not set up the HTTP client: {e}"));
            }
        };
        let account = Arc::new(Account::new(url.clone(), Arc::new(transport)));
        account.set_credentials(Credentials::new(
            self.params.user_id.clone(),
            self.params.app_password.clone(),
        ));

        // The account is only added, saved and given a sync folder once the credentials have
        // been checked against the server, so that a failed setup leaves nothing behind.
        if self.params.app_password.is_empty() {
            // Nothing to authenticate with, so the server cannot be asked for the dav user
            // either. Store what was given and let the user log in from the client later on.
            account.set_dav_user(&self.params.user_id);
            return self.handle_success(&account, &url);
        }

        // fetchUserName()
        let (json, status_code, _reply) =
            nc_dav::jobs::json_api(&account, "/ocs/v1.php/cloud/user", &JobOptions::default())
                .await;
        if status_code != 100 {
            return SetupStatus::failure("Could not fetch username.");
        }
        let data = json.as_ref().map(|j| &j["ocs"]["data"]);
        account.set_dav_user(data.and_then(|d| d["id"].as_str()).unwrap_or(""));
        account.set_dav_display_name(data.and_then(|d| d["display-name"].as_str()).unwrap_or(""));

        // checkLastModifiedWithPropfind()
        let opts = JobOptions {
            ignore_credential_failure: true,
            // There is custom redirect handling in the error handler,
            // so don't automatically follow redirects.
            follow_redirects: false,
            ..JobOptions::default()
        };
        match nc_dav::jobs::propfind(&account, "/", &["getlastmodified"], &opts).await {
            Ok(_) => self.handle_success(&account, &url),
            Err(reply) => self.handle_propfind_failure(&account, &url, &reply),
        }
    }

    /// `accountSetupFromCommandLinePropfindHandleFailure()`.
    fn handle_propfind_failure(
        &self,
        account: &Arc<Account>,
        url: &ServerUrl,
        reply: &nc_dav::Reply,
    ) -> SetupStatus {
        let redirect = reply.raw_header_str("Location");
        let error_msg = if (300..400).contains(&reply.http_status) && !redirect.is_empty() {
            log::info!(target: LOG, "Authed request was redirected to {redirect}");
            // Upstream then retries the PROPFIND on the redirected URL when its path
            // ends with the DAV path, but reports the failure below in the same call:
            // the event loop quits before the retry finishes.
            format!(
                "The authenticated request to the server was redirected to \"{}\". The URL is bad, the server is misconfigured.",
                html_escaped(&redirect)
            )
        } else if reply.error == NetworkError::ContentNotFoundError {
            // A 404 is actually a success: we were authorized to know that the folder does
            // not exist. It will be created later...
            return self.handle_success(account, url);
        } else if reply.error != NetworkError::NoError {
            if reply.error == NetworkError::AuthenticationRequiredError {
                format!(
                    "Access forbidden by server. To verify that you have proper access, <a href=\"{}\">click here</a> to access the service with your browser.",
                    html_escaped(&url.to_credential_free_string())
                )
            } else {
                reply.error_string_parsing_body()
            }
        } else {
            // Something else went wrong, maybe the response was 200 but with invalid data.
            "There was an invalid response to an authenticated WebDAV request".to_owned()
        };
        SetupStatus::failure(format!(
            "Account {} setup from command line failed with error: {error_msg}.",
            display_name(&self.params.user_id, url)
        ))
    }

    /// `accountSetupFromCommandLinePropfindHandleSuccess()`: adds and saves
    /// the account (with its app password), then sets up the folder.
    fn handle_success(&self, account: &Arc<Account>, url: &ServerUrl) -> SetupStatus {
        let path = self.ctx.location.config_file.clone();
        let url_string = url.to_credential_free_string();
        let user_id = self.params.user_id.clone();
        let saved = Settings::modify(&path, |s| {
            let load = load_accounts(s);
            let id = generate_free_account_id(s, &load.blocked_ids);
            let mut def = AccountDefinition::new_webflow(&id, &url_string, &user_id);
            def.dav_user = account.dav_user();
            def.display_name = account.dav_display_name();
            save_account(s, &def);
            def
        });
        let mut def = match saved {
            Ok((def, _)) => def,
            Err(e) => {
                return SetupStatus::failure(format!(
                    "Account {} setup from command line failed with error: {e}.",
                    display_name(&self.params.user_id, url)
                ));
            }
        };
        let resolver = CredentialResolver::new(self.ctx.location.mode.clone(), self.store);
        if !self.params.app_password.is_empty() {
            match resolver.store_app_password(
                &mut def,
                &AppPassword::new(self.params.app_password.clone()),
                &self.ctx.location.state_dir,
            ) {
                Ok(source) => {
                    log::info!(target: LOG, "Account {}: credentials stored in {source}", def.id);
                    let saved = def.clone();
                    if let Err(e) = Settings::modify(&path, |s| save_account(s, &saved)) {
                        log::warn!(target: LOG, "Could not save account {}: {e}", def.id);
                    }
                }
                Err(e) => {
                    // The keychain write failed: upstream finishes the setup without them
                    // (after its timeout); the account has to be authenticated again.
                    log::warn!(target: LOG, "Could not store the credentials of account {}: {e}, the account may have to be authenticated again", def.id);
                }
            }
        }
        let display = display_name(&self.params.user_id, url);
        if !self.params.local_dir_path.is_empty() {
            self.setup_local_sync_folder(&def, &resolver, &display)
        } else {
            log::info!(target: LOG, "Set up a new account without a folder.");
            SetupStatus::success(format!(
                "Account {display} setup from command line success."
            ))
        }
    }

    /// `AccountManager::removeAccountState` of a failed setup: the account
    /// leaves the configuration. Its locally stored app password goes too
    /// (upstream leaves the keychain entry behind; the app password itself
    /// stays valid on the server so that a retry is possible).
    fn remove_account(&self, def: &AccountDefinition, resolver: &CredentialResolver<'_>) {
        let id = def.id.clone();
        if let Err(e) = Settings::modify(&self.ctx.location.config_file, |s| remove_account(s, &id))
        {
            log::warn!(target: LOG, "Could not remove account {id}: {e}");
        }
        if let Err(e) = resolver.forget(def) {
            log::warn!(target: LOG, "Could not forget the credentials of account {id}: {e}");
        }
    }

    /// `setupLocalSyncFolder(accountState)`.
    fn setup_local_sync_folder(
        &self,
        account: &AccountDefinition,
        resolver: &CredentialResolver<'_>,
        display: &str,
    ) -> SetupStatus {
        let local_dir = &self.params.local_dir_path;
        let absolute = std::path::absolute(local_dir)
            .map(|p| clean_path(&p.to_string_lossy()))
            .unwrap_or_else(|_| local_dir.clone());
        if !Path::new(&absolute).exists() {
            log::info!(target: LOG, "Creating folder {local_dir}");
            if std::fs::create_dir_all(&absolute).is_err() {
                self.remove_account(account, resolver);
                return SetupStatus::failure(format!(
                    "Folder creation failed. Could not create local folder {local_dir}"
                ));
            }
        }
        // FileSystem::setFolderMinimumPermissions does nothing on Linux;
        // Utility::setupFavLink (GTK bookmarks) is desktop integration.
        let remote = if self.params.remote_dir_path.is_empty() {
            "/"
        } else {
            &self.params.remote_dir_path
        };
        let account_id = account.id.clone();
        let saved = Settings::modify(&self.ctx.location.config_file, |s| {
            let accounts = load_accounts(s);
            let folders = load_folders(s, &accounts.accounts);
            let mut def = FolderDefinition {
                local_path: FolderDefinition::prepare_local_path(&absolute),
                target_path: FolderDefinition::prepare_target_path(remote),
                // folderMan->ignoreHiddenFiles(): the first folder's setting, false
                // when there is none.
                ignore_hidden_files: folders
                    .folders
                    .first()
                    .is_some_and(|f| f.definition.ignore_hidden_files),
                ..FolderDefinition::default()
            };
            // folderMan->map().size(), made unique by addFolderInternal.
            let wanted = folders.folders.len().to_string();
            def.alias = allocate_alias(&wanted, &all_aliases(s), &folders.blocked_aliases);
            def.journal_path = def.default_journal_path(account);
            let clean = clean_path(&folder_canonical_path(&def.local_path));
            let one_account_only = !folders
                .folders
                .iter()
                .any(|f| clean_path(&folder_canonical_path(&f.definition.local_path)) == clean);
            let journal = def.absolute_journal_path();
            if !crate::folder_man::ensure_journal_gone(&journal) {
                return None;
            }
            let group = choose_group(false, one_account_only, one_account_only);
            save_folder(s, &account_id, group, &def);
            Some(def)
        });
        match saved {
            Ok((Some(def), _)) => {
                let journal = SyncJournalDb::new(def.absolute_journal_path());
                journal
                    .set_selective_sync_list(SelectiveSyncListType::WhiteList, &["/".to_owned()]);
                log::info!(target: LOG, "Folder {} setup from command line success.", def.local_path);
                SetupStatus::success(format!(
                    "Account {display} setup from command line success."
                ))
            }
            _ => {
                // Drop the account again, but keep the app password valid: it was passed in on the
                // command line and revoking it would make a retry impossible.
                self.remove_account(account, resolver);
                SetupStatus::failure(format!(
                    "Account {display} setup from command line failed, due to folder creation failure."
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_qurl_validity() {
        assert!(!qurl_is_valid(""));
        assert!(qurl_is_valid("http://127.0.0.1:1"));
        assert!(qurl_is_valid("https://cloud.example.com/nextcloud"));
        assert!(qurl_is_valid("https://alice@cloud.example.com:8443/"));
        assert!(qurl_is_valid("http://[::1]:8080"));
        // The quotes of testUserIdAndServerUrlWithoutAppPasswordEntersProvisionMode:
        // a relative URL whose first segment contains ':'.
        assert!(!qurl_is_valid("\"http://127.0.0.1:1\""));
        assert!(qurl_is_valid("cloud.example.com"));
        assert!(!qurl_is_valid("http://127.0.0.1:99999"));
        assert!(!qurl_is_valid("http://exa\"mple.com"));
    }

    #[test]
    fn derived_display_name() {
        let u = ServerUrl::parse("http://127.0.0.1:1").unwrap();
        assert_eq!(display_name("alice", &u), "alice@127.0.0.1:1");
        let u = ServerUrl::parse("https://cloud.example.com:443/x").unwrap();
        assert_eq!(display_name("alice", &u), "alice@cloud.example.com");
    }
}
