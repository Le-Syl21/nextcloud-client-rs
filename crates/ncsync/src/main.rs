// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/cmd/cmd.cpp` and `src/cmd/netrcparser.cpp`
// (nextcloudcmd, nextcloud/desktop v34.0.5).

//! `ncsync`: unofficial command line sync client for Nextcloud.
//!
//! `ncsync sync <source_dir> <server_url>` does what `nextcloudcmd` does:
//! one complete sync of a local folder with a remote folder, retried while
//! the engine asks for another sync (`--max-sync-retries`). The options of
//! `nextcloudcmd` are accepted with the same meaning where they make sense
//! for this client.

mod config_cmds;
mod control_cmds;
mod netrc;
mod provisioning;

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, CommandFactory as _, Parser, Subcommand};
use nc_dav::{
    Account, Capabilities, Credentials, HttpClientOptions, HttpTransport, JobOptions, ServerUrl,
};
use nc_journal::journal::{SelectiveSyncListType, SyncJournalDb};
use nc_sync::{AnotherSyncNeeded, SyncEngine, SyncOptions};

/// Unofficial Nextcloud sync client (command line).
#[derive(Parser, Debug)]
#[command(name = "ncsync", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Parsed once at start-up: the size of the sync options does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
enum Command {
    /// Synchronize a local folder with a Nextcloud folder once (like nextcloudcmd).
    Sync(SyncArgs),
    /// Manage the accounts of the ncsyncd configuration.
    Account {
        #[command(subcommand)]
        cmd: config_cmds::AccountCommand,
        #[command(flatten)]
        config: config_cmds::ConfigArgs,
    },
    /// Manage the folders of the ncsyncd configuration.
    Folder {
        #[command(subcommand)]
        cmd: config_cmds::FolderCommand,
        #[command(flatten)]
        config: config_cmds::ConfigArgs,
    },
    /// Move a folder from the official desktop client's configuration to
    /// ncsyncd's (the official client must not be running).
    Takeover {
        #[command(flatten)]
        args: config_cmds::TakeoverArgs,
        #[command(flatten)]
        config: config_cmds::ConfigArgs,
    },
    /// Give a taken-over folder back to the official desktop client.
    Handback {
        #[command(flatten)]
        args: config_cmds::HandbackArgs,
        #[command(flatten)]
        config: config_cmds::ConfigArgs,
    },
    /// Show the accounts and folders of a running ncsyncd.
    Status(control_cmds::ControlArgs),
    /// Pause a folder (all folders by default) of a running ncsyncd.
    Pause(control_cmds::FolderArg),
    /// Resume a paused folder (all folders by default).
    Resume(control_cmds::FolderArg),
    /// Sync a folder now (all folders by default), like the desktop
    /// client's "Sync now".
    SyncNow(control_cmds::FolderArg),
}

#[derive(Args, Debug)]
#[command(disable_help_flag = true)]
struct SyncArgs {
    /// Local folder to synchronize (must exist).
    source_dir: Option<String>,
    /// Base URL of the server, e.g. https://cloud.example.com (user and
    /// password may be given in the URL).
    server_url: Option<String>,

    /// Don't be so verbose.
    #[arg(short = 's', long)]
    silent: bool,
    /// Use a HTTP proxy (http://server:port).
    #[arg(long = "httpproxy", value_name = "PROXY")]
    http_proxy: Option<String>,
    /// Trust the TLS certificate of the server (accept invalid certificates).
    #[arg(long)]
    trust: bool,
    /// Exclude list file.
    #[arg(long, value_name = "FILE")]
    exclude: Option<String>,
    /// File containing the list of unsynced remote folders (selective sync).
    #[arg(long = "unsyncedfolders", value_name = "FILE")]
    unsynced_folders: Option<String>,
    /// Use NAME as the login name.
    #[arg(short = 'u', long, value_name = "NAME")]
    user: Option<String>,
    /// Use PASS as password (prefer an app password, or --password-file / $NC_PASSWORD).
    #[arg(short = 'p', long, value_name = "PASS")]
    password: Option<String>,
    /// Read the password (or app password) from FILE (first line).
    #[arg(long = "password-file", value_name = "FILE")]
    password_file: Option<String>,
    /// Use netrc(5) for login.
    #[arg(short = 'n')]
    netrc: bool,
    /// Do not block execution with interaction; read $NC_USER and
    /// $NC_PASSWORD if not set by other means.
    #[arg(long = "non-interactive")]
    non_interactive: bool,
    /// Retries maximum N times (default 3).
    #[arg(long = "max-sync-retries", value_name = "N", default_value_t = 3)]
    max_sync_retries: i32,
    /// Limit the upload speed of files to N KB/s.
    #[arg(long, value_name = "N", default_value_t = 0)]
    uplimit: i32,
    /// Limit the download speed of files to N KB/s.
    #[arg(long, value_name = "N", default_value_t = 0)]
    downlimit: i32,
    /// Sync hidden files, do not ignore them (the default, as in nextcloudcmd).
    #[arg(short = 'h')]
    sync_hidden: bool,
    /// More verbose logging.
    #[arg(long)]
    logdebug: bool,
    /// Path to a folder on the remote server (default /).
    #[arg(long, value_name = "PATH", default_value = "/")]
    path: String,
    /// Use the given configuration directory: DIR/ncsyncd.cfg instead of
    /// ~/.config/ncsyncd/ncsyncd.cfg (created if missing).
    #[arg(long, value_name = "DIR")]
    confdir: Option<String>,

    /// Do not create new remote folders bigger than SIZE MB locally; they
    /// are added to the selective sync blacklist (like the desktop client's
    /// "ask before syncing folders larger than" setting).
    #[arg(long = "new-big-folder-size-limit", value_name = "MB")]
    new_big_folder_size_limit: Option<i64>,
    /// Do not sync new external storages; they are added to the selective
    /// sync blacklist (like the desktop client's setting).
    #[arg(long = "confirm-external-storage")]
    confirm_external_storage: bool,
    /// Abort the sync when it would delete all files (on either side) or
    /// more than --max-deletions files.
    #[arg(long = "abort-on-mass-deletion")]
    abort_on_mass_deletion: bool,
    /// Threshold of --abort-on-mass-deletion (default 100, like the
    /// desktop client).
    #[arg(long = "max-deletions", value_name = "N", default_value_t = 100)]
    max_deletions: i64,

    /// The user ID to configure. With --userid, `ncsync sync` adds an
    /// account (and a folder) to the ncsyncd configuration instead of
    /// syncing: `ncsync sync --userid USER --serverurl URL [--apppassword PASS]`.
    #[arg(long, value_name = "USER", help_heading = PROVISIONING)]
    userid: Option<String>,
    /// The app password for authentication (optional; without it the
    /// login is done with Login Flow v2: open the link shown, or scan its
    /// QR code; with --non-interactive an app password is required).
    #[arg(long, value_name = "PASS", help_heading = PROVISIONING)]
    apppassword: Option<String>,
    /// The base URL of the Nextcloud server.
    #[arg(long, value_name = "URL", help_heading = PROVISIONING)]
    serverurl: Option<String>,
    /// Local folder path for sync (optional, default ~/Nextcloud).
    #[arg(long, value_name = "PATH", help_heading = PROVISIONING)]
    localdirpath: Option<String>,
    /// Remote folder path to sync, default /.
    #[arg(long, value_name = "PATH", help_heading = PROVISIONING)]
    remotedirpath: Option<String>,
    /// Enable virtual files (1) or disable (0). Virtual files are not
    /// supported: 1 is refused.
    #[arg(long, value_name = "0|1", help_heading = PROVISIONING)]
    isvfsenabled: Option<String>,

    /// Print help.
    #[arg(long, action = clap::ArgAction::Help)]
    help: Option<bool>,
}

const PROVISIONING: &str = "Account provisioning options (non-interactive setup)";

/// `help()`: the help of `ncsync sync`, on stdout.
fn print_sync_help() {
    let mut cmd = Cli::command();
    cmd.build();
    if let Some(sync) = cmd.find_subcommand_mut("sync") {
        let _ = sync.print_long_help();
    }
}

fn main() -> ExitCode {
    // cmd.cpp runs Utility::expandCommandLineOptionValues on its arguments.
    let args = expand_command_line_option_values(std::env::args().collect());
    // parseOptions: "--userid flag present means no positional args required".
    if args.get(1).is_some_and(|a| a == "sync") && args[2..].iter().any(|a| a == "--userid") {
        return provisioning::run(&args[1..]);
    }
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Sync(args) => run_sync(args),
        Command::Account { cmd, config } => config_cmds::run_account(cmd, config),
        Command::Folder { cmd, config } => config_cmds::run_folder(cmd, config),
        Command::Takeover { args, config } => config_cmds::run_takeover(args, config),
        Command::Handback { args, config } => config_cmds::run_handback(args, config),
        Command::Status(c) => control_cmds::run_status(c),
        Command::Pause(a) => control_cmds::run_folder_request(
            a,
            |folder| nc_daemon::control::Request::Pause { folder },
            "paused",
        ),
        Command::Resume(a) => control_cmds::run_folder_request(
            a,
            |folder| nc_daemon::control::Request::Resume { folder },
            "resumed",
        ),
        Command::SyncNow(a) => control_cmds::run_folder_request(
            a,
            |folder| nc_daemon::control::Request::SyncNow { folder },
            "sync scheduled",
        ),
    }
}

/// Prompts for a value on the terminal.
fn prompt(question: &str) -> String {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim_end_matches(['\n', '\r']).to_owned()
}

/// `queryPassword(user)`.
fn query_password(user: &str) -> String {
    rpassword::prompt_password(format!("Password for account with username {user}: "))
        .unwrap_or_default()
}

fn init_logging(silent: bool, logdebug: bool) {
    let level = if silent {
        log::LevelFilter::Off
    } else if logdebug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    env_logger::Builder::new()
        .filter_level(level)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [ {} {} ]:\t{}",
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}

/// `selectiveSyncFixup`: when the selective sync list changed, rediscover
/// the affected folders.
fn selective_sync_fixup(journal: &SyncJournalDb, new_list: &[String]) {
    let Ok(old) = journal.get_selective_sync_list(SelectiveSyncListType::BlackList) else {
        return;
    };
    let old_set: std::collections::HashSet<&String> = old.iter().collect();
    let new_set: std::collections::HashSet<&String> = new_list.iter().collect();
    for changed in old_set.symmetric_difference(&new_set) {
        journal.schedule_path_for_remote_discovery(changed.as_bytes());
    }
    journal.set_selective_sync_list(SelectiveSyncListType::BlackList, new_list);
}

/// `ConfigFile::excludeFileFromSystem()`: `/etc/Nextcloud/sync-exclude.lst`.
/// When it does not exist (this client is not installed with it), the
/// default list of the reference upstream version is written to the user's
/// cache directory and used instead, so the behaviour matches an installed
/// `nextcloudcmd`.
fn system_exclude_file() -> String {
    let system = "/etc/Nextcloud/sync-exclude.lst";
    if std::path::Path::new(system).exists() {
        return system.to_owned();
    }
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))
        .unwrap_or_else(|| ".".to_owned());
    let dir = format!("{base}/ncsync");
    let path = format!("{dir}/sync-exclude.lst");
    let up_to_date = std::fs::read(&path)
        .is_ok_and(|c| c == nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST.as_bytes());
    if !up_to_date {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(&path, nc_journal::exclude::DEFAULT_SYNC_EXCLUDE_LST) {
            log::warn!("Could not write the default exclude list to {path}: {e}");
        }
    }
    path
}

fn run_sync(args: SyncArgs) -> ExitCode {
    let (Some(source_dir), Some(server_url)) = (args.source_dir.clone(), args.server_url.clone())
    else {
        // argCount < 3: help, HelpMode (exit 0).
        print_sync_help();
        return ExitCode::SUCCESS;
    };
    init_logging(args.silent, args.logdebug);
    let config_file = args.confdir.as_deref().and_then(provisioning::set_conf_dir);
    let _ = args.sync_hidden; // the default: hidden files are synced

    // Source directory
    let mut source_dir = source_dir;
    if !source_dir.ends_with('/') {
        source_dir.push('/');
    }
    let Ok(abs) = std::fs::canonicalize(&source_dir) else {
        eprintln!("Source dir '{source_dir}' does not exist.");
        return ExitCode::FAILURE;
    };
    let mut source_dir = abs.to_string_lossy().into_owned();
    if !source_dir.ends_with('/') {
        source_dir.push('/');
    }

    // Server URL
    let target = server_url.trim_end_matches(['/', '\\']);
    let Ok(mut host_url) = ServerUrl::parse(target) else {
        eprintln!("Invalid server URL '{server_url}'");
        return ExitCode::FAILURE;
    };
    let lower_path = host_url.path.to_lowercase();
    if lower_path.contains("/webdav") || lower_path.contains("/dav") {
        log::warn!("Dav or webdav in server URL.");
        eprintln!(
            "Error! Please specify only the base URL of your host with username and password. Example:"
        );
        eprintln!("https://username:password@cloud.example.com");
        return ExitCode::FAILURE;
    }

    // Order of retrieval attempt (later attempts override earlier ones):
    // URL, options, netrc (if enabled), prompt (if interactive) or environment.
    let mut user = host_url.user.clone();
    let mut password = host_url.password.clone();
    if let Some(u) = &args.user {
        user = u.clone();
    }
    if let Some(p) = &args.password {
        password = p.clone();
    }
    if let Some(f) = &args.password_file {
        match std::fs::read_to_string(f) {
            Ok(c) => password = c.lines().next().unwrap_or("").to_owned(),
            Err(e) => {
                eprintln!("Could not read the password file {f}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if args.netrc {
        let mut parser = netrc::NetrcParser::new(None);
        if parser.parse() {
            let (u, p) = parser.find(&host_url.host);
            user = u;
            password = p;
        }
    }
    if !args.non_interactive {
        if user.is_empty() {
            user = prompt("Please enter username: ");
        }
        if password.is_empty() {
            password = query_password(&user);
        }
    } else {
        if user.is_empty() {
            user = std::env::var("NC_USER").unwrap_or_default();
        }
        if password.is_empty() {
            password = std::env::var("NC_PASSWORD").unwrap_or_default();
        }
    }
    // Find the folder and the original owncloud url
    host_url.scheme = host_url.scheme.replace("owncloud", "http");
    let credential_free_url = host_url.to_credential_free_string();
    let folder = args.path.clone();

    let transport = match HttpTransport::new(&HttpClientOptions {
        trust_invalid_certificates: args.trust,
        proxy: args.http_proxy.clone(),
    }) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Could not set up the HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let account = Arc::new(Account::new(host_url, Arc::new(transport)));
    account.set_credentials(Credentials::new(user.clone(), password));
    // ClientStatusReporting (when the server enables it): its database in
    // the configuration directory, like nextcloudcmd's ConfigFile().configPath().
    let config_dir = config_file
        .or_else(|| {
            nc_daemon::config_file::ConfigLocation::user()
                .ok()
                .map(|l| l.config_file)
        })
        .and_then(|f| f.parent().map(std::path::Path::to_path_buf));
    if let Some(dir) = config_dir {
        nc_sync::client_status_reporting::install(&account, dir);
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not start the event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        sync_main(args, account, source_dir, credential_free_url, folder, user).await
    })
}

async fn sync_main(
    args: SyncArgs,
    account: Arc<Account>,
    source_dir: String,
    credential_free_url: String,
    folder: String,
    user: String,
) -> ExitCode {
    let opts = JobOptions::default();
    // CheckServerJob
    match nc_dav::jobs::check_server(&account, &opts).await {
        Ok(info) => {
            // only set server version if not empty
            if let Some(v) = info.get("version").and_then(|v| v.as_str())
                && !v.is_empty()
            {
                account.set_server_version(v);
            }
        }
        Err(reply) => {
            log::warn!("status.php: {}", reply.error_string());
            println!("Error connecting to server for status");
            return ExitCode::FAILURE;
        }
    }
    // Capabilities
    let (json, _status, reply) =
        nc_dav::jobs::json_api(&account, "ocs/v1.php/cloud/capabilities", &opts).await;
    if !reply.is_ok() {
        log::warn!("capabilities: {}", reply.error_string());
        println!("Error connecting to server");
        return ExitCode::FAILURE;
    }
    if let Some(json) = json {
        let caps = json["ocs"]["data"]["capabilities"].clone();
        log::debug!("Server capabilities {caps}");
        if let Some(v) = caps["core"]["status"]["version"].as_str()
            && !v.is_empty()
        {
            account.set_server_version(v);
        }
        account.set_capabilities(Capabilities::from_json(caps));
    }
    // User
    let (json, _status, _reply) =
        nc_dav::jobs::json_api(&account, "ocs/v1.php/cloud/user", &opts).await;
    if let Some(json) = json {
        let data = &json["ocs"]["data"];
        if let Some(id) = data["id"].as_str() {
            account.set_dav_user(id);
        }
        if let Some(name) = data["display-name"].as_str() {
            account.set_dav_display_name(name);
        }
    }

    let mut restart_count = 0;
    loop {
        let mut selective_sync_list: Vec<String> = Vec::new();
        if let Some(f) = &args.unsynced_folders {
            match std::fs::read_to_string(f) {
                Ok(content) => {
                    // filter out empty lines and comments
                    for line in content.split('\n') {
                        if line.trim().is_empty() || line.starts_with('#') {
                            continue;
                        }
                        let mut l = line.to_owned();
                        if !l.ends_with('/') {
                            l.push('/');
                        }
                        selective_sync_list.push(l);
                    }
                }
                Err(_) => {
                    log::error!("Could not open file containing the list of unsynced folders: {f}")
                }
            }
        }
        let db_path = format!(
            "{source_dir}{}",
            nc_journal::journal::make_db_name(
                std::path::Path::new(&source_dir),
                &credential_free_url,
                &folder,
                &user
            )
        );
        let journal = Arc::new(SyncJournalDb::new(db_path));
        if !selective_sync_list.is_empty() {
            selective_sync_fixup(&journal, &selective_sync_list);
        }
        let mut sync_options = SyncOptions::default();
        sync_options.fill_from_environment_variables();
        sync_options.verify_chunk_sizes();
        // much lower age than the default since this utility is usually made
        // to be run right after a change in the tests
        sync_options.minimum_file_age_for_upload = std::time::Duration::ZERO;
        if let Some(mb) = args.new_big_folder_size_limit {
            sync_options.new_big_folder_size_limit = mb * 1000 * 1000;
        }
        sync_options.confirm_external_storage = args.confirm_external_storage;
        // The mass deletion prompt is skipped for nextcloudcmd (isCmd); with
        // --abort-on-mass-deletion it is answered with "cancel".
        sync_options.set_is_cmd(!args.abort_on_mass_deletion);
        let mut engine = SyncEngine::new(
            account.clone(),
            &source_dir,
            sync_options,
            &folder,
            journal.clone(),
        );
        engine.set_ignore_hidden_files(false);
        engine.set_network_limits(args.uplimit * 1000, args.downlimit * 1000);
        if args.abort_on_mass_deletion {
            engine.prompt_delete_files = true;
            engine.delete_files_threshold = args.max_deletions;
            engine.callbacks().about_to_remove_all_files = Some(Box::new(|dir| {
                log::warn!(
                    "Too many files would be deleted ({dir:?}): aborting the sync (--abort-on-mass-deletion)"
                );
                true
            }));
        }
        {
            let journal = journal.clone();
            engine.callbacks().new_big_folder = Some(Box::new(move |new_folder, is_external| {
                new_big_folder_discovered(&journal, new_folder, is_external);
            }));
        }
        engine.callbacks().sync_error = Some(Box::new(|error, _| {
            log::warn!("Sync error: {error}");
        }));

        // Exclude lists
        let has_user_exclude_file = args.exclude.is_some();
        let system_exclude = system_exclude_file();
        if let Some(ex) = &args.exclude {
            if !std::path::Path::new(ex).exists() {
                eprintln!("Exclude list file supplied via --exclude does not exist: {ex}");
                return ExitCode::FAILURE;
            }
            // Upstream keys an exclude file by its own directory unless it is
            // named sync-exclude.lst (then by the sync folder), and only
            // looks up keys inside the sync folder: any other file outside
            // the folder is loaded but never matches. Kept as is (ISO), with
            // a warning upstream does not print.
            let name = ex.rsplit('/').next().unwrap_or(ex);
            let dir = std::fs::canonicalize(ex)
                .ok()
                .and_then(|p| p.parent().map(|d| format!("{}/", d.to_string_lossy())));
            if !name.eq_ignore_ascii_case("sync-exclude.lst")
                && !dir.is_some_and(|d| d.starts_with(&source_dir))
            {
                log::warn!(
                    "The exclude list {ex} is outside the sync folder and not named \
                     sync-exclude.lst: like nextcloudcmd, its patterns will not apply. \
                     Name it sync-exclude.lst to apply it to the whole folder."
                );
            }
            engine.excluded_files().add_exclude_file_path(ex);
        }
        if !has_user_exclude_file || std::path::Path::new(&system_exclude).exists() {
            engine
                .excluded_files()
                .add_exclude_file_path(&system_exclude);
        }
        if !engine.excluded_files().reload_exclude_files() {
            eprintln!("Cannot load system exclude list or list supplied via --exclude");
            return ExitCode::FAILURE;
        }

        let ok = engine.sync_once().await;
        let result_code = if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
        if engine.is_another_sync_needed() != AnotherSyncNeeded::NoFollowUpSync {
            if restart_count < args.max_sync_retries {
                restart_count += 1;
                log::debug!("Restarting Sync, because another sync is needed {restart_count}");
                continue;
            }
            log::warn!(
                "Another sync is needed, but not done because restart count is exceeded {restart_count}"
            );
        }
        return result_code;
    }
}

/// `Folder::slotNewBigFolderDiscovered`: big new folders (and external
/// storages when confirmation is wanted) go to the blacklist and the
/// undecided list.
fn new_big_folder_discovered(journal: &SyncJournalDb, new_f: &str, is_external: bool) {
    let new_folder = nc_journal::utility::trailing_slash_path(new_f);
    if let (Ok(mut blacklist), Ok(whitelist)) = (
        journal.get_selective_sync_list(SelectiveSyncListType::BlackList),
        journal.get_selective_sync_list(SelectiveSyncListType::WhiteList),
    ) && !blacklist.contains(&new_folder)
        && !whitelist.contains(&new_folder)
    {
        blacklist.push(new_folder.clone());
        journal.set_selective_sync_list(SelectiveSyncListType::BlackList, &blacklist);
    }
    if let Ok(mut undecided) = journal.get_selective_sync_list(SelectiveSyncListType::UndecidedList)
    {
        if !undecided.contains(&new_folder) {
            undecided.push(new_folder.clone());
            journal.set_selective_sync_list(SelectiveSyncListType::UndecidedList, &undecided);
        }
        if is_external {
            log::warn!(
                "A folder from an external storage has been added: {new_f}. It is not synced."
            );
        } else {
            log::warn!(
                "A new folder larger than the size limit has been added: {new_f}. It is not synced."
            );
        }
    }
}

/// `Utility::expandCommandLineOptionValues`: splits `--option=value` into
/// `--option value` (only at the first `=`; an empty value is dropped so the
/// option is left without one). Anything else is passed through untouched.
fn expand_command_line_option_values(arguments: Vec<String>) -> Vec<String> {
    let mut expanded = Vec::with_capacity(arguments.len());
    for argument in arguments {
        // Anything that is not a long option is passed through untouched: paths and custom
        // URI scheme arguments may legitimately contain a '='.
        let separator = if argument.starts_with("--") {
            argument.find('=')
        } else {
            None
        };
        match separator {
            Some(sep) if sep >= 3 => {
                let value = argument[sep + 1..].to_owned();
                expanded.push(argument[..sep].to_owned());
                if !value.is_empty() {
                    expanded.push(value);
                }
            }
            _ => expanded.push(argument),
        }
    }
    expanded
}

// Port of upstream test/testutility.cpp testExpandCommandLineOptionValues
// (CC0-1.0, SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud
// contributors, 2014 ownCloud GmbH).
#[cfg(test)]
mod tests {
    use super::expand_command_line_option_values as expand;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn test_expand_command_line_option_values() {
        let rows: &[(&str, &[&str], &[&str])] = &[
            ("empty", &[], &[]),
            (
                "separate value is left alone",
                &["nextcloud", "--userid", "alice"],
                &["nextcloud", "--userid", "alice"],
            ),
            (
                "inline value is split off",
                &["nextcloud", "--userid=alice"],
                &["nextcloud", "--userid", "alice"],
            ),
            (
                "both spellings can be mixed",
                &[
                    "nextcloud",
                    "--userid=alice",
                    "--serverurl",
                    "https://example.com",
                ],
                &[
                    "nextcloud",
                    "--userid",
                    "alice",
                    "--serverurl",
                    "https://example.com",
                ],
            ),
            // Only the first '=' separates, so query strings and passwords stay intact.
            (
                "value keeps its own equal signs",
                &["--serverurl=https://example.com/?a=b&c=d"],
                &["--serverurl", "https://example.com/?a=b&c=d"],
            ),
            (
                "password keeps its own equal signs",
                &["--apppassword=pa=ss"],
                &["--apppassword", "pa=ss"],
            ),
            // An empty inline value must not become an empty argument: the option is left
            // without a value so that the parsers report their usual "not specified" error.
            (
                "empty inline value yields no value",
                &["--userid="],
                &["--userid"],
            ),
            // Anything that is not a long option is passed through untouched, because local
            // paths and custom URI scheme arguments may legitimately contain a '='.
            (
                "path with an equal sign is untouched",
                &["/home/alice/a=b/file.txt"],
                &["/home/alice/a=b/file.txt"],
            ),
            (
                "uri scheme argument is untouched",
                &["nc://open/file?id=42"],
                &["nc://open/file?id=42"],
            ),
            ("short option is untouched", &["-u=alice"], &["-u=alice"]),
            ("bare double dash is untouched", &["--"], &["--"]),
            (
                "option without a name is untouched",
                &["--=alice"],
                &["--=alice"],
            ),
        ];
        for (name, arguments, expected) in rows {
            assert_eq!(expand(v(arguments)), v(expected), "row {name}");
        }
    }
}
