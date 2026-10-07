// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The configuration subcommands of `ncsync` (`account`, `folder`,
//! `takeover`, `handback`): argument parsing and printing; the logic is in
//! `nc_daemon::manage`.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use nc_daemon::config_file::{ConfigLocation, ServiceMode, official_client_config_file};
use nc_daemon::credentials::{AppPassword, SecretStore};
use nc_daemon::flow2auth;
use nc_daemon::manage::{self, Context, ManageError, TakeoverCredentials};
use nc_dav::HttpClientOptions;

/// Which configuration file to use.
#[derive(Args, Debug, Clone)]
pub struct ConfigArgs {
    /// Configuration file (default: ~/.config/ncsyncd/ncsyncd.cfg, or
    /// /var/lib/ncsyncd/USER/ncsyncd.cfg with --instance USER).
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    /// Work on the system instance ncsyncd@USER, which runs as USER
    /// (credentials from systemd or a password file, never the keyring).
    /// Run as root, the command gives what it writes to USER.
    #[arg(long, value_name = "USER", global = true)]
    instance: Option<String>,
}

/// HTTP options for commands that talk to the server.
#[derive(Args, Debug, Clone)]
pub struct HttpArgs {
    /// Trust the TLS certificate of the server (accept invalid certificates).
    #[arg(long)]
    trust: bool,
    /// Use a HTTP proxy (http://server:port).
    #[arg(long = "httpproxy", value_name = "PROXY")]
    http_proxy: Option<String>,
}

/// Output format of the list commands.
#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
pub enum AccountCommand {
    /// Add an account (Login Flow v2 in a browser by default).
    Add(AccountAddArgs),
    /// List the accounts.
    List(ListArgs),
}

#[derive(Args, Debug)]
pub struct AccountAddArgs {
    /// Base URL of the server, e.g. https://cloud.example.com
    server_url: String,
    /// Use the app password in FILE (first line) instead of Login Flow v2;
    /// needs --user.
    #[arg(long = "app-password-file", value_name = "FILE", requires = "user")]
    app_password_file: Option<PathBuf>,
    /// Login name for --app-password-file.
    #[arg(short = 'u', long, value_name = "NAME")]
    user: Option<String>,
    #[command(flatten)]
    http: HttpArgs,
}

#[derive(Subcommand, Debug)]
pub enum FolderCommand {
    /// Add a folder to synchronize.
    Add(FolderAddArgs),
    /// List the folders.
    List(ListArgs),
    /// Remove a folder (its journal is deleted; files are kept).
    Remove(FolderRemoveArgs),
}

#[derive(Args, Debug)]
pub struct FolderAddArgs {
    /// Local folder (created if missing).
    local_dir: String,
    /// Remote folder (default /).
    #[arg(long, value_name = "PATH", default_value = "/")]
    remote: String,
    /// Account id (needed when several accounts are configured).
    #[arg(long, value_name = "ID")]
    account: Option<String>,
}

#[derive(Args, Debug)]
pub struct FolderRemoveArgs {
    /// Folder alias (see `ncsync folder list`).
    alias: String,
    /// Also remove a folder taken over from the official client, keeping
    /// its journal.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
pub struct TakeoverArgs {
    /// The official client's folder: its local path or its alias.
    folder: String,
    /// The official client's configuration (default ~/.config/Nextcloud/nextcloud.cfg).
    #[arg(long, value_name = "CFG")]
    from: Option<PathBuf>,
    /// If the official client's app password cannot be read from the
    /// keyring, use the one in FILE instead of Login Flow v2.
    #[arg(long = "app-password-file", value_name = "FILE")]
    app_password_file: Option<PathBuf>,
    #[command(flatten)]
    http: HttpArgs,
}

#[derive(Args, Debug)]
pub struct HandbackArgs {
    /// Our alias of the taken-over folder.
    alias: String,
    /// The official client's configuration (default: the one it was taken from).
    #[arg(long, value_name = "CFG")]
    to: Option<PathBuf>,
}

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();
}

fn context(config: &ConfigArgs, http: Option<&HttpArgs>) -> Result<Context, String> {
    let location = match &config.instance {
        Some(name) => ConfigLocation::system(name),
        None => ConfigLocation::user(),
    }
    .map_err(|e| e.to_string())?;
    let location = match &config.config {
        Some(p) => location.with_config_file(p),
        None => location,
    };
    let http = http
        .map(|h| HttpClientOptions {
            trust_invalid_certificates: h.trust,
            proxy: h.http_proxy.clone(),
        })
        .unwrap_or_default();
    Ok(Context { location, http })
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Could not start the event loop: {e}"))
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("Error: {msg}");
    ExitCode::FAILURE
}

/// The end of a command that wrote the configuration: on a system instance,
/// run as root, gives the state directory (and `extra`, a local folder the
/// command created) to the instance user.
fn hand_over(ctx: &Context, extra: Option<&std::path::Path>) -> ExitCode {
    match nc_daemon::instance::hand_to_instance_user(&ctx.location, extra) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

fn read_app_password(path: &PathBuf) -> Result<AppPassword, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "Could not read the app password file {}: {e}",
            path.display()
        )
    })?;
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(AppPassword::new(first))
}

/// Runs Login Flow v2 in the terminal: prints the link and its QR code,
/// waits for the browser.
async fn login_in_terminal(
    ctx: &Context,
    server_url: &str,
) -> Result<flow2auth::LoginResult, ManageError> {
    let flow = manage::start_login(ctx, server_url).await?;
    println!("Open this link in a browser and grant access (Login Flow v2):\n");
    println!("  {}\n", flow.login_url);
    if let Some(qr) = flow2auth::render_qr(&flow.login_url) {
        println!("{qr}");
    }
    println!("Waiting for the login (Ctrl+C to cancel)...");
    let _ = std::io::stdout().flush();
    manage::finish_login(ctx, &flow, |e| eprintln!("Warning: {e}")).await
}

fn keyring(ctx: &Context) -> Option<Box<dyn SecretStore>> {
    ctx.secret_store()
        .map(|s| Box::new(s) as Box<dyn SecretStore>)
}

pub fn run_account(cmd: AccountCommand, config: ConfigArgs) -> ExitCode {
    init_logging();
    match cmd {
        AccountCommand::List(list) => {
            let ctx = match context(&config, None) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let settings = match ctx.load() {
                Ok(s) => s,
                Err(e) => return fail(e),
            };
            let accounts = manage::list_accounts(&settings);
            if list.json {
                let v: Vec<_> = accounts
                    .iter()
                    .map(|a| {
                        serde_json::json!({
                            "id": a.id, "url": a.url, "user": a.user, "davUser": a.dav_user,
                            "displayName": a.display_name, "serverVersion": a.server_version,
                            "authType": a.auth_type,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::Array(v));
            } else if accounts.is_empty() {
                println!("No account configured (ncsync account add <server_url>).");
            } else {
                for a in accounts {
                    let name = if a.display_name.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", a.display_name)
                    };
                    println!("{}\t{}@{}{name}", a.id, a.user, a.url);
                }
            }
            ExitCode::SUCCESS
        }
        AccountCommand::Add(args) => {
            let ctx = match context(&config, Some(&args.http)) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let rt = match runtime() {
                Ok(r) => r,
                Err(e) => return fail(e),
            };
            let store = keyring(&ctx);
            let result = rt.block_on(async {
                let (server, user, secret) = match &args.app_password_file {
                    Some(f) => {
                        let secret = read_app_password(f).map_err(ManageError::Usage)?;
                        (
                            args.server_url.clone(),
                            args.user.clone().unwrap_or_default(),
                            secret,
                        )
                    }
                    None => {
                        let r = login_in_terminal(&ctx, &args.server_url).await?;
                        (r.server_url, r.login_name, r.app_password)
                    }
                };
                manage::add_account(&ctx, &server, &user, &secret, store.as_deref()).await
            });
            match result {
                Ok(added) => {
                    let verb = if added.existed { "Updated" } else { "Added" };
                    println!(
                        "{verb} account {}: {}@{} (credentials: {})",
                        added.account.id,
                        added.account.credentials_user(),
                        added.account.url,
                        added.credentials
                    );
                    if let ServiceMode::System { .. } = ctx.location.mode {
                        let name = nc_daemon::credentials::systemd_credential_name(
                            &ctx.location.mode,
                            &added.account.id,
                        );
                        println!(
                            "For a system instance, prefer a systemd credential (the unit imports ncsyncd-<user>-*): systemd-creds encrypt --name={name} <file> /etc/credstore.encrypted/{name}, then remove ncsyncd_passwordFile from the account."
                        );
                    }
                    hand_over(&ctx, None)
                }
                Err(e) => fail(e),
            }
        }
    }
}

pub fn run_folder(cmd: FolderCommand, config: ConfigArgs) -> ExitCode {
    init_logging();
    let ctx = match context(&config, None) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    match cmd {
        FolderCommand::List(list) => {
            let settings = match ctx.load() {
                Ok(s) => s,
                Err(e) => return fail(e),
            };
            let folders = manage::list_folders(&settings);
            if list.json {
                let v: Vec<_> = folders
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "alias": f.alias, "accountId": f.account_id, "localPath": f.local_path,
                            "remotePath": f.remote_path, "journalPath": f.journal_path,
                            "paused": f.paused, "group": f.group,
                            "virtualFilesMode": f.virtual_files_mode, "takenOver": f.taken_over,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::Array(v));
            } else if folders.is_empty() {
                println!("No folder configured (ncsync folder add <local_dir>).");
            } else {
                for f in folders {
                    let mut flags = Vec::new();
                    if f.paused {
                        flags.push("paused");
                    }
                    if f.taken_over {
                        flags.push("taken over");
                    }
                    if f.virtual_files_mode != "off" {
                        flags.push("virtual files: not supported");
                    }
                    let flags = if flags.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", flags.join(", "))
                    };
                    println!(
                        "{}\taccount {}\t{} -> {}{flags}",
                        f.alias, f.account_id, f.local_path, f.remote_path
                    );
                }
            }
            ExitCode::SUCCESS
        }
        FolderCommand::Add(args) => {
            let local = std::path::absolute(&args.local_dir).ok();
            let created = local.as_ref().filter(|p| !p.exists()).cloned();
            match manage::add_folder(&ctx, &args.local_dir, &args.remote, args.account.as_deref()) {
                Ok(def) => {
                    println!(
                        "Added folder {}: {} -> {}",
                        def.alias, def.local_path, def.target_path
                    );
                    hand_over(&ctx, created.as_deref())
                }
                Err(e) => fail(e),
            }
        }
        FolderCommand::Remove(args) => match manage::remove_folder(&ctx, &args.alias, args.force) {
            Ok(def) => {
                println!(
                    "Removed folder {} ({}); its files are kept.",
                    args.alias, def.local_path
                );
                hand_over(&ctx, None)
            }
            Err(e) => fail(e),
        },
    }
}

pub fn run_takeover(args: TakeoverArgs, config: ConfigArgs) -> ExitCode {
    init_logging();
    let ctx = match context(&config, Some(&args.http)) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let from = match args
        .from
        .clone()
        .map_or_else(official_client_config_file, Ok)
    {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    println!(
        "The official Nextcloud client must not run during the takeover, and must not sync this folder afterwards."
    );
    let store = keyring(&ctx);
    let (out, creds) = match manage::take_over(&ctx, &from, &args.folder, store.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    println!(
        "Took over folder {} ({} -> {}) from {} as account {}.",
        out.alias,
        out.folder.local_path,
        out.folder.target_path,
        from.display(),
        out.account_id
    );
    match creds {
        TakeoverCredentials::Existing => {}
        TakeoverCredentials::FromOfficialClient(src) => {
            println!("Copied the official client's app password ({src}).");
        }
        TakeoverCredentials::Missing(mut account) => {
            println!("The official client's app password was not found in the keyring.");
            let secret = match &args.app_password_file {
                Some(f) => read_app_password(f),
                None => runtime().and_then(|rt| {
                    rt.block_on(login_in_terminal(&ctx, &account.url))
                        .map(|r| r.app_password)
                        .map_err(|e| e.to_string())
                }),
            };
            let stored = secret.and_then(|s| {
                manage::store_account_password(&ctx, &mut account, &s, store.as_deref())
                    .map_err(|e| e.to_string())
            });
            match stored {
                Ok(src) => println!("Stored the app password ({src})."),
                Err(e) => {
                    eprintln!(
                        "Warning: no credentials for account {} ({e})\nRun `ncsync account add {}` before starting ncsyncd.",
                        account.id, account.url
                    );
                }
            }
        }
    }
    println!("Hand it back with: ncsync handback {}", out.alias);
    hand_over(&ctx, None)
}

pub fn run_handback(args: HandbackArgs, config: ConfigArgs) -> ExitCode {
    init_logging();
    let ctx = match context(&config, None) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let store = keyring(&ctx);
    match manage::hand_back(&ctx, &args.alias, args.to.as_deref(), store.as_deref()) {
        Ok(out) => {
            println!(
                "Handed folder {} ({}) back to the official client as {} (account {}).",
                args.alias, out.folder.local_path, out.official_alias, out.official_account_id
            );
            if let Some(acc) = out.removed_account {
                println!("Removed account {} (it had no other folder).", acc.id);
            }
            hand_over(&ctx, None)
        }
        Err(e) => fail(e),
    }
}
