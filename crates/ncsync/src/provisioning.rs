// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the provisioning mode of upstream `src/cmd/cmd.cpp`
// (`parseOptions`, `setupAccountsAndFolders`, `main`) and of
// `src/gui/accountsetupcommandlinemanager.cpp` (nextcloud/desktop v34.0.5).

//! `ncsync sync --userid USER --serverurl URL [--apppassword PASS]
//! [--localdirpath DIR] [--remotedirpath PATH] [--isvfsenabled 0|1]
//! [--confdir DIR]`: `nextcloudcmd`'s account provisioning. The account
//! and its folder go to the ncsyncd configuration (`DIR/ncsyncd.cfg` with
//! `--confdir`), the app password to the keyring or a password file like
//! `ncsync account add`. The setup itself is
//! [`nc_daemon::account_setup`].
//!
//! The command line is parsed by hand like upstream's `parseOptions`, not
//! by clap: an option it does not know, or one without its value, prints
//! the help and exits with 0 (`HelpMode`).
//!
//! Deliberate divergences: without an app password the account is
//! not stored without credentials (the GUI's "log in later"), the user
//! logs in with Login Flow v2 in the terminal instead, and
//! `--non-interactive` then refuses the setup; `--trust` and `--httpproxy`
//! apply to the setup's requests (upstream ignores both there).
//!
//! Exit codes: 255 for a rejected command line or setup (`return -1`), 1
//! when the setup fails once it reached the server, 0 on success.

use std::path::PathBuf;
use std::process::ExitCode;

use nc_daemon::account_config::load_accounts;
use nc_daemon::account_setup::{
    AccountSetupFromCommandLineJob, MissingAppPassword, SetupParams, qurl_is_valid,
};
use nc_daemon::config_file::ConfigLocation;
use nc_daemon::credentials::SecretStore;
use nc_daemon::folder_definition::load_folders;
use nc_daemon::manage::Context;
use nc_dav::HttpClientOptions;

/// `AccountSetupCommandLineManager`.
#[derive(Debug, Default)]
struct AccountSetupCommandLineManager {
    app_password: String,
    user_id: String,
    server_url: String,
    remote_dir_path: String,
    local_dir_path: String,
    is_vfs_enabled: bool,
}

/// `QString::toInt()`: an integer, 0 when it does not parse.
fn qstring_to_int(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}

impl AccountSetupCommandLineManager {
    /// `parseCommandlineOption(option, optionsIterator, errorMessage)`.
    fn parse_commandline_option<'a>(
        &mut self,
        option: &str,
        it: &mut std::iter::Peekable<impl Iterator<Item = &'a String>>,
    ) -> Result<(), String> {
        let field = match option {
            "--apppassword" => (&mut self.app_password, "apppassword not specified"),
            "--localdirpath" => (&mut self.local_dir_path, "basedir not specified"),
            "--remotedirpath" => (&mut self.remote_dir_path, "remotedir not specified"),
            "--serverurl" => (&mut self.server_url, "serverurl not specified"),
            "--userid" => (&mut self.user_id, "userid not specified"),
            "--isvfsenabled" => {
                return match it.next_if(|n| !n.starts_with("--")) {
                    Some(v) => {
                        self.is_vfs_enabled = qstring_to_int(v) != 0;
                        Ok(())
                    }
                    None => Err("isvfsenabled not specified".to_owned()),
                };
            }
            _ => return Err(String::new()),
        };
        match it.next_if(|n| !n.starts_with("--")) {
            Some(v) => {
                *field.0 = v.clone();
                Ok(())
            }
            None => Err(field.1.to_owned()),
        }
    }

    /// `isCommandLineParsed()`: `!_userId.isEmpty() && _serverUrl.isValid()`.
    fn is_command_line_parsed(&self) -> bool {
        !self.user_id.is_empty() && qurl_is_valid(&self.server_url)
    }
}

/// The `nextcloudcmd` options that also apply in provisioning mode.
#[derive(Debug, Default)]
struct CmdOptions {
    silent: bool,
    logdebug: bool,
    config_directory: Option<String>,
    /// `--trust`: accept an invalid TLS certificate (divergence: upstream
    /// ignores it in provisioning mode).
    trust: bool,
    /// `--httpproxy`: the HTTP proxy of every request of the setup, Login
    /// Flow v2 included (divergence: upstream ignores it in provisioning
    /// mode).
    http_proxy: Option<String>,
    /// `--non-interactive`: without an app password, refuse instead of
    /// starting Login Flow v2.
    non_interactive: bool,
    /// `--password-file` (an ncsync extension): the app password, so that
    /// it does not have to be on the command line.
    password_file: Option<String>,
}

/// `ConfigFile::setConfDir(dir)`: the directory is created when missing;
/// returns the configuration file in it, or prints that the option is
/// disabled.
pub fn set_conf_dir(dir: &str) -> Option<PathBuf> {
    if dir.is_empty() {
        return None;
    }
    let path = PathBuf::from(dir);
    if !path.exists() {
        let _ = std::fs::create_dir_all(&path);
    }
    if path.is_dir() {
        let absolute = std::path::absolute(&path).unwrap_or(path);
        log::info!(target: "nextcloud.sync.configfile", "Using custom config dir  {}", absolute.display());
        return Some(absolute.join("ncsyncd.cfg"));
    }
    eprintln!("Invalid confdir '{dir}', disabling.");
    None
}

/// Runs the provisioning mode; `args[0]` is the subcommand (`sync`), like
/// the program name upstream skips.
pub fn run(args: &[String]) -> ExitCode {
    let mut options = CmdOptions::default();
    let mut manager = AccountSetupCommandLineManager::default();
    let mut it = args.iter().skip(1).peekable();
    while let Some(option) = it.next() {
        let option = option.as_str();
        let next_is_value = |it: &mut std::iter::Peekable<_>| {
            it.peek().is_some_and(|n: &&String| !n.starts_with('-'))
        };
        match option {
            "-u" | "--user" | "-p" | "--password" | "--exclude" | "--unsyncedfolders"
            | "--max-sync-retries" | "--uplimit" | "--downlimit" | "--path"
                if next_is_value(&mut it) =>
            {
                // Parsed and unused in provisioning mode, like upstream.
                it.next();
            }
            "--httpproxy" if next_is_value(&mut it) => {
                options.http_proxy = it.next().cloned();
            }
            "--password-file" if next_is_value(&mut it) => {
                options.password_file = it.next().cloned();
            }
            "-s" | "--silent" => options.silent = true,
            "--trust" => options.trust = true,
            "--non-interactive" => options.non_interactive = true,
            "-n" | "-h" => {}
            "--logdebug" => options.logdebug = true,
            "--confdir" if it.peek().is_some_and(|n| !n.starts_with("--")) => {
                options.config_directory = it.next().cloned();
            }
            _ => {
                if manager.parse_commandline_option(option, &mut it).is_err() {
                    crate::print_sync_help();
                    return ExitCode::SUCCESS;
                }
            }
        }
    }

    crate::init_logging(options.silent, options.logdebug);
    let config_file = options.config_directory.as_deref().and_then(set_conf_dir);
    if let Some(file) = &options.password_file {
        match std::fs::read_to_string(file) {
            Ok(c) => manager.app_password = c.lines().next().unwrap_or("").trim().to_owned(),
            Err(e) => {
                eprintln!("Could not read the password file {file}: {e}");
                return ExitCode::from(255);
            }
        }
    }

    // setupAccountsAndFolders()
    let location = match ConfigLocation::user() {
        Ok(l) => l,
        Err(e) => {
            log::warn!("restoring existing user accounts failed: {e}");
            log::warn!(
                "Restoring existing accounts failed. Skip creation of a new account. See prior messages for a detailed error."
            );
            return ExitCode::from(255);
        }
    };
    let location = match config_file {
        Some(f) => location.with_config_file(f),
        None => location,
    };
    // Upstream's provisioning applies neither --trust nor --httpproxy; both
    // are applied here (divergence), to every request of the setup.
    let ctx = Context {
        location,
        http: HttpClientOptions {
            trust_invalid_certificates: options.trust,
            proxy: options.http_proxy,
        },
    };
    let settings = match ctx.load() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("restoring existing user accounts failed: {e}");
            log::warn!(
                "Restoring existing accounts failed. Skip creation of a new account. See prior messages for a detailed error."
            );
            return ExitCode::from(255);
        }
    };
    let accounts = load_accounts(&settings).accounts;
    let folders = load_folders(&settings, &accounts).folders.len();
    let pretty: Vec<String> = accounts
        .iter()
        .map(|a| format!("- {}@{}", a.effective_dav_user(), a.url))
        .collect();
    log::warn!("{folders} folder(s) migrated");
    log::warn!(
        "{} account(s) migrated: {}",
        accounts.len(),
        pretty.join("\n")
    );

    if !manager.is_command_line_parsed() {
        log::warn!("Missing mandatory command line options for provisioning mode");
        crate::print_sync_help();
        return ExitCode::from(255);
    }
    if manager.is_vfs_enabled {
        // Upstream creates the folder in the best available virtual files
        // mode; ncsyncd refuses folders in virtual files mode, so nothing is
        // written.
        eprintln!(
            "Virtual files are not supported by ncsync: --isvfsenabled 1 is refused (use --isvfsenabled 0)."
        );
        log::warn!("Creation of the account failed. See prior messages for a detailed error.");
        return ExitCode::from(255);
    }

    log::info!(target: "nextcloud.gui.accountsetupcommandlinemanager", "Command line has been parsed and account setup parameters have been found. Attempting setup a new account {}...", manager.user_id);
    let keyring = ctx.secret_store();
    let store = keyring.as_ref().map(|s| s as &dyn SecretStore);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not start the event loop: {e}");
            return ExitCode::from(255);
        }
    };
    let params = SetupParams {
        app_password: manager.app_password,
        user_id: manager.user_id,
        server_url: manager.server_url,
        local_dir_path: manager.local_dir_path,
        remote_dir_path: manager.remote_dir_path,
    };
    let show_link = |url: &str| crate::config_cmds::print_login_link(url);
    let on_error = |e: &nc_daemon::flow2auth::Flow2Error| eprintln!("Warning: {e}");
    let missing_app_password = if options.non_interactive {
        MissingAppPassword::Refuse
    } else {
        MissingAppPassword::LoginFlow {
            show_link: &show_link,
            on_error: &on_error,
            timeout: nc_daemon::flow2auth::DEFAULT_LOGIN_TIMEOUT,
        }
    };
    let job = AccountSetupFromCommandLineJob::new(&ctx, store, params)
        .with_missing_app_password(missing_app_password);
    let status = runtime.block_on(job.run());
    if status.is_failure {
        log::warn!(target: "nextcloud.gui.accountsetupcommandlinejob", "{}", status.message);
        eprintln!("{}", status.message);
    } else {
        log::info!(target: "nextcloud.gui.accountsetupcommandlinejob", "{}", status.message);
        println!("{}", status.message);
    }
    if status.rejected {
        log::warn!("Creation of the account failed. See prior messages for a detailed error.");
    }
    ExitCode::from(status.exit_code())
}
