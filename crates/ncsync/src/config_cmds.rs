// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The configuration subcommands of `ncsync` (`account`, `folder`,
//! `takeover`, `handback`): argument parsing and printing; the logic is in
//! `nc_daemon::manage`. A running `ncsyncd` is told to reload its
//! configuration after a change (best effort), and the selective sync edits
//! go through it when it runs (it holds the journals).

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use nc_daemon::config_file::{ConfigLocation, ServiceMode, official_client_config_file};
use nc_daemon::control::Request;
use nc_daemon::credentials::{AppPassword, KEYRING_SERVICE, SecretStore};
use nc_daemon::flow2auth;
use nc_daemon::manage::{self, Context, ManageError, RevokeOutcome, TakeoverCredentials};
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
    /// Remove an account: its credentials are forgotten and its app
    /// password is revoked on the server.
    Remove(AccountRemoveArgs),
}

#[derive(Args, Debug)]
pub struct AccountRemoveArgs {
    /// Account id (see `ncsync account list`).
    id: String,
    /// Also remove the folders of the account (their files are kept; the
    /// journals of folders taken over from the official client too).
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    http: HttpArgs,
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
    /// Stop syncing remote subfolders of a folder (selective sync, "Choose
    /// what to sync"): their local copies are removed at the next sync,
    /// except files changed locally since the last sync.
    Exclude(SelectiveSyncArgs),
    /// Sync excluded remote subfolders of a folder again.
    Include(SelectiveSyncArgs),
    /// List the excluded remote subfolders of a folder.
    Excluded(ExcludedArgs),
}

#[derive(Args, Debug)]
pub struct SelectiveSyncArgs {
    /// Folder alias or local path (see `ncsync folder list`).
    folder: String,
    /// Remote subfolders, relative to the folder's remote path
    /// (e.g. Photos/2020), or local paths inside the folder.
    #[arg(required = true, value_name = "SUBFOLDER")]
    paths: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ExcludedArgs {
    /// Folder alias or local path (see `ncsync folder list`).
    folder: String,
    #[command(flatten)]
    list: ListArgs,
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

/// Shows a Login Flow v2 link in the terminal, with its QR code.
pub(crate) fn print_login_link(login_url: &str) {
    println!("Open this link in a browser and grant access (Login Flow v2):\n");
    println!("  {login_url}\n");
    if let Some(qr) = flow2auth::render_qr(login_url) {
        println!("{qr}");
    }
    println!("Waiting for the login (Ctrl+C to cancel)...");
    let _ = std::io::stdout().flush();
}

/// Runs Login Flow v2 in the terminal: prints the link and its QR code,
/// waits for the browser.
async fn login_in_terminal(
    ctx: &Context,
    server_url: &str,
) -> Result<flow2auth::LoginResult, ManageError> {
    let flow = manage::start_login(ctx, server_url).await?;
    print_login_link(&flow.login_url);
    manage::finish_login(ctx, &flow, |e| eprintln!("Warning: {e}")).await
}

fn keyring(ctx: &Context) -> Option<Box<dyn SecretStore>> {
    ctx.secret_store()
        .map(|s| Box::new(s) as Box<dyn SecretStore>)
}

/// The control socket of the daemon using this configuration.
fn socket_path(ctx: &Context) -> PathBuf {
    let instance = match &ctx.location.mode {
        ServiceMode::System { instance } => Some(instance.as_str()),
        ServiceMode::User => None,
    };
    nc_daemon::control::default_socket_path(instance)
}

/// What the running daemon made of a request.
enum DaemonAnswer {
    /// No daemon listens on the socket.
    NotRunning,
    Done(serde_json::Value),
    Refused(String),
}

fn ask_daemon(ctx: &Context, request: &Request) -> DaemonAnswer {
    let path = socket_path(ctx);
    match nc_daemon::control::request(&path, request) {
        Ok(v) if v["ok"] == true => DaemonAnswer::Done(v),
        Ok(v) => DaemonAnswer::Refused(v["error"].as_str().unwrap_or("error").to_owned()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            DaemonAnswer::NotRunning
        }
        Err(e) => DaemonAnswer::Refused(format!("cannot reach it at {}: {e}", path.display())),
    }
}

/// Tells a running daemon to reload its configuration (best effort);
/// `wipe` are removed folders whose journal it deletes.
fn reload_daemon(ctx: &Context, wipe: &[String]) -> DaemonAnswer {
    let config = std::path::absolute(&ctx.location.config_file)
        .unwrap_or_else(|_| ctx.location.config_file.clone());
    ask_daemon(
        ctx,
        &Request::Reload {
            config: Some(config.to_string_lossy().into_owned()),
            wipe: wipe.to_vec(),
        },
    )
}

fn print_reload(answer: &DaemonAnswer) {
    match answer {
        DaemonAnswer::NotRunning => {
            println!("ncsyncd is not running: it will use the change when it starts.")
        }
        DaemonAnswer::Done(_) => println!("The running ncsyncd picked up the change."),
        DaemonAnswer::Refused(e) => eprintln!(
            "Warning: the running ncsyncd did not reload its configuration ({e}); restart it to use the change."
        ),
    }
}

/// Whether the daemon's reload removed (and so closed or wiped) a folder.
fn daemon_removed(answer: &DaemonAnswer, alias: &str) -> bool {
    match answer {
        DaemonAnswer::Done(v) => v["removed_folders"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == alias)),
        _ => false,
    }
}

/// Removed folders: the daemon wipes the journals of the folders it had
/// loaded; the others are wiped here.
fn reload_after_removal(ctx: &Context, removed: &[manage::RemovedFolder]) -> DaemonAnswer {
    let wipe: Vec<String> = removed
        .iter()
        .filter(|f| !f.taken_over)
        .map(|f| f.definition.alias.clone())
        .collect();
    let answer = reload_daemon(ctx, &wipe);
    for f in removed {
        if !daemon_removed(&answer, &f.definition.alias) {
            manage::wipe_journal(f);
        }
    }
    answer
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
                    let code = hand_over(&ctx, None);
                    print_reload(&reload_daemon(&ctx, &[]));
                    code
                }
                Err(e) => fail(e),
            }
        }
        AccountCommand::Remove(args) => run_account_remove(args, &config),
    }
}

/// `account remove`: `AccountManager::deleteAccount` (the account and,
/// with `--force`, its folders leave the configuration; the running daemon
/// drops them), then `Account::deleteAppToken` and
/// `credentials()->forgetSensitiveData()`.
fn run_account_remove(args: AccountRemoveArgs, config: &ConfigArgs) -> ExitCode {
    let ctx = match context(config, Some(&args.http)) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let store = keyring(&ctx);
    let removed = match manage::remove_account(&ctx, &args.id, args.force, store.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let acc = &removed.account;
    println!(
        "Removed account {}: {}@{}.",
        acc.id,
        acc.credentials_user(),
        acc.url
    );
    for f in &removed.folders {
        println!(
            "Removed folder {} ({}); its files are kept{}.",
            f.definition.alias,
            f.definition.local_path,
            if f.taken_over {
                ", and its journal too (taken over from the official client)"
            } else {
                ""
            }
        );
    }
    let code = hand_over(&ctx, None);
    let answer = reload_after_removal(&ctx, &removed.folders);
    print_reload(&answer);
    let official = official_client_config_file().ok();
    let revoked = runtime().map(|rt| {
        rt.block_on(manage::revoke_app_password(
            &ctx,
            &removed,
            official.as_deref(),
            store.as_deref(),
        ))
    });
    match revoked {
        Ok(RevokeOutcome::Revoked) => println!("Revoked its app password on the server."),
        Ok(RevokeOutcome::SharedWithOfficialClient) => println!(
            "Kept its app password on the server: the official client has the same account and may use the same app password (a takeover copies it)."
        ),
        Ok(RevokeOutcome::NoPassword) => {
            println!("No app password was found for it: nothing to revoke on the server.")
        }
        Ok(RevokeOutcome::Failed(status)) => eprintln!(
            "Warning: could not revoke its app password on the server (HTTP status {status}); revoke it in the server's personal security settings."
        ),
        Err(e) => eprintln!("Warning: could not revoke its app password on the server: {e}"),
    }
    match manage::forget_credentials(&ctx, acc, store.as_deref()) {
        Ok(f) => {
            if let Some(key) = f.keyring_item {
                println!("Deleted its keyring item (service {KEYRING_SERVICE}, {key}).");
            }
            if let Some(p) = f.removed_password_file {
                println!("Deleted its password file {}.", p.display());
            }
            if let Some(p) = f.kept_password_file {
                println!(
                    "Its password file {} was left in place: delete it if nothing else uses it.",
                    p.display()
                );
            }
            if let Some(p) = f.kept_systemd_credential {
                println!(
                    "The systemd credential {} was left in place: remove it from the credential store yourself.",
                    p.display()
                );
            }
        }
        Err(e) => eprintln!("Warning: could not delete its stored credentials: {e}"),
    }
    if let ServiceMode::System { .. } = ctx.location.mode {
        let name = nc_daemon::credentials::systemd_credential_name(&ctx.location.mode, &acc.id);
        println!(
            "If the instance read a systemd credential {name} (/etc/credstore*), remove it yourself."
        );
    }
    code
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
                    let code = hand_over(&ctx, created.as_deref());
                    print_reload(&reload_daemon(&ctx, &[]));
                    code
                }
                Err(e) => fail(e),
            }
        }
        FolderCommand::Remove(args) => match manage::remove_folder(&ctx, &args.alias, args.force) {
            Ok(removed) => {
                println!(
                    "Removed folder {} ({}); its files are kept.",
                    args.alias, removed.definition.local_path
                );
                let code = hand_over(&ctx, None);
                let answer = reload_after_removal(&ctx, std::slice::from_ref(&removed));
                print_reload(&answer);
                code
            }
            Err(e) => fail(e),
        },
        FolderCommand::Exclude(args) => run_selective_sync(&ctx, &args.folder, &args.paths, true),
        FolderCommand::Include(args) => run_selective_sync(&ctx, &args.folder, &args.paths, false),
        FolderCommand::Excluded(args) => {
            let folder = match ctx
                .load()
                .and_then(|s| manage::find_folder(&s, &args.folder))
            {
                Ok(f) => f,
                Err(e) => return fail(e),
            };
            let list = match selective_sync(&ctx, &folder, &[], &[]) {
                Ok((change, _)) => change.black_list,
                Err(e) => return fail(e),
            };
            if args.list.json {
                println!("{}", serde_json::json!(list));
            } else if list.is_empty() {
                println!("No subfolder of folder {} is excluded.", folder.alias);
            } else {
                for p in list {
                    println!("{p}");
                }
            }
            ExitCode::SUCCESS
        }
    }
}

/// The selective sync edit, through the running daemon when it has the
/// folder (it holds the journal), on the journal directly otherwise.
/// Returns the change and whether the daemon made it.
fn selective_sync(
    ctx: &Context,
    folder: &manage::FolderSummary,
    exclude: &[String],
    include: &[String],
) -> Result<(nc_daemon::selective_sync::SelectiveSyncChange, bool), String> {
    let request = Request::SelectiveSync {
        folder: folder.local_path.clone(),
        exclude: exclude.to_vec(),
        include: include.to_vec(),
    };
    match ask_daemon(ctx, &request) {
        DaemonAnswer::Done(v) => {
            let strings = |k: &str| -> Vec<String> {
                v[k].as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            };
            Ok((
                nc_daemon::selective_sync::SelectiveSyncChange {
                    black_list: strings("blacklist"),
                    changes: strings("changes"),
                    white_list_added: strings("whitelist_added"),
                },
                true,
            ))
        }
        // The daemon does not sync this folder: its journal is free.
        DaemonAnswer::Refused(e) if e.starts_with("no such folder") => {
            manage::apply_selective_sync(folder, exclude, include)
                .map(|c| (c, false))
                .map_err(|e| e.to_string())
        }
        DaemonAnswer::NotRunning => manage::apply_selective_sync(folder, exclude, include)
            .map(|c| (c, false))
            .map_err(|e| e.to_string()),
        DaemonAnswer::Refused(e) => Err(format!("ncsyncd: {e}")),
    }
}

/// `folder exclude|include`.
fn run_selective_sync(ctx: &Context, folder: &str, paths: &[String], exclude: bool) -> ExitCode {
    let folder = match ctx.load().and_then(|s| manage::find_folder(&s, folder)) {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let paths: Vec<String> = paths
        .iter()
        .map(|p| manage::selective_sync_path(&folder, p))
        .collect();
    let (ex, inc) = if exclude {
        (paths, Vec::new())
    } else {
        (Vec::new(), paths)
    };
    let (change, by_daemon) = match selective_sync(ctx, &folder, &ex, &inc) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    if change.changes.is_empty() {
        println!("Nothing to change.");
        return ExitCode::SUCCESS;
    }
    for c in &change.changes {
        if change.black_list.contains(c) {
            println!("Excluded {c}");
        } else {
            println!("Included {c}");
        }
    }
    if by_daemon {
        println!("The running ncsyncd syncs folder {} now.", folder.alias);
    } else {
        println!(
            "ncsyncd is not running: folder {} will be synced when it starts.",
            folder.alias
        );
    }
    if exclude {
        println!(
            "The local copies of the excluded folders are removed by that sync, except files changed locally since the last sync."
        );
    }
    ExitCode::SUCCESS
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
    let code = hand_over(&ctx, None);
    print_reload(&reload_daemon(&ctx, &[]));
    code
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
            let code = hand_over(&ctx, None);
            // The daemon stops syncing the folder and closes its journal
            // (kept for the official client) before it answers.
            print_reload(&reload_daemon(&ctx, &[]));
            code
        }
        Err(e) => fail(e),
    }
}
