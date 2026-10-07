// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `ncsyncd`: unofficial headless sync daemon for Nextcloud.
//!
//! Syncs every folder of its configuration (`nextcloud.cfg` format) like
//! the official desktop client does: folder watcher, etag polling every
//! 30 s or notify_push, scheduled and follow-up syncs with back-off. Runs as
//! a systemd user service (credentials in the keyring) or as an instance of
//! the `ncsyncd@.service` system template (credentials from systemd).
//! `ncsync status|pause|resume|sync-now` talk to it over its control socket.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use nc_daemon::config_file::{ConfigFile, ConfigLocation, ServiceMode};
use nc_daemon::folder_man::FolderMan;
use nc_daemon::settings::Settings;
use nc_daemon::startup::{self, FileSettingsStore, Options, ReloadHooks};

/// Unofficial Nextcloud sync daemon.
#[derive(Parser, Debug)]
#[command(name = "ncsyncd", version, about, long_about = None)]
struct Cli {
    /// Run the system instance ncsyncd@USER, as the user USER
    /// (/var/lib/ncsyncd/USER/ncsyncd.cfg, credentials from systemd or
    /// password files).
    #[arg(long, value_name = "USER")]
    instance: Option<String>,
    /// Configuration file (default ~/.config/ncsyncd/ncsyncd.cfg, or
    /// /var/lib/ncsyncd/USER/ncsyncd.cfg with --instance USER).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Control socket (default $XDG_RUNTIME_DIR/ncsyncd/control.sock, or
    /// /run/ncsyncd/NAME/control.sock with --instance).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// The official client's configuration, to refuse folders it also syncs
    /// (default ~/.config/Nextcloud/nextcloud.cfg).
    #[arg(long = "official-config", value_name = "PATH")]
    official_config: Option<PathBuf>,
    /// Trust the TLS certificates of the servers (accept invalid certificates).
    #[arg(long)]
    trust: bool,
    /// Use a HTTP proxy for every account (http://server:port).
    #[arg(long = "httpproxy", value_name = "PROXY")]
    http_proxy: Option<String>,
    /// Do not watch the folders (local changes are then found every
    /// fullLocalDiscoveryInterval, 1 h by default).
    #[arg(long = "no-watch")]
    no_watch: bool,
    /// Do not use notify_push (remote changes are found by polling).
    #[arg(long = "no-push")]
    no_push: bool,
    /// More verbose logging (RUST_LOG also works).
    #[arg(long)]
    logdebug: bool,
}

fn init_logging(debug: bool) {
    let mut b = env_logger::Builder::new();
    b.filter_level(if debug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
    if let Ok(spec) = std::env::var("RUST_LOG") {
        b.parse_filters(&spec);
    }
    // journald adds its own timestamps.
    let under_systemd = std::env::var_os("JOURNAL_STREAM").is_some();
    b.format(move |buf, record| {
        if under_systemd {
            writeln!(
                buf,
                "[ {} {} ]:\t{}",
                record.level(),
                record.target(),
                record.args()
            )
        } else {
            writeln!(
                buf,
                "{} [ {} {} ]:\t{}",
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                record.args()
            )
        }
    });
    b.init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.logdebug);
    let location = match &cli.instance {
        Some(i) => ConfigLocation::system(i),
        None => ConfigLocation::user(),
    };
    let mut location = match location {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ncsyncd: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(c) = &cli.config {
        location = location.with_config_file(c);
    }
    if let Err(e) = nc_daemon::instance::check_daemon_user(&location) {
        eprintln!("ncsyncd: {e}");
        return ExitCode::FAILURE;
    }
    let instance = match &location.mode {
        ServiceMode::System { instance } => Some(instance.clone()),
        ServiceMode::User => None,
    };
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| nc_daemon::control::default_socket_path(instance.as_deref()));
    let options = Options {
        location: location.clone(),
        trust: cli.trust,
        http_proxy: cli.http_proxy.clone(),
        no_watch: cli.no_watch,
        no_push: cli.no_push,
        official_config: cli.official_config.clone(),
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ncsyncd: cannot start the event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run(options, socket))
}

async fn run(options: Options, socket: PathBuf) -> ExitCode {
    let settings = match Settings::load(&options.location.config_file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "ncsyncd: cannot read {}: {e} (create it with `ncsync account add`)",
                options.location.config_file.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let cfg = ConfigFile::new(&settings);
    let fm_settings = startup::folder_man_settings(&cfg, startup::exclude_files(&options.location));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watcher = if options.no_watch {
        nc_daemon::folder_man::no_watcher()
    } else {
        nc_daemon::daemon::inotify_watcher_factory()
    };
    let mut fm = FolderMan::new(
        &tx,
        fm_settings,
        Box::new(FileSettingsStore::new(&options.location.config_file)),
        watcher,
    );
    if let Err(e) = startup::setup(&mut fm, &options) {
        eprintln!("ncsyncd: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = nc_daemon::control::serve(&socket, tx.clone()) {
        eprintln!("ncsyncd: control socket {}: {e}", socket.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = nc_daemon::daemon::install_signal_handlers(&tx) {
        eprintln!("ncsyncd: signal handlers: {e}");
        return ExitCode::FAILURE;
    }
    let accounts = fm
        .account_states()
        .map(|a| (a.id().to_owned(), a.account().clone()))
        .collect();
    let mut hooks = ReloadHooks {
        options: options.clone(),
        accounts,
    };
    log::info!(target: "nextcloud.daemon", "ncsyncd {} started: {} accounts, {} folders", env!("CARGO_PKG_VERSION"), fm.account_states().count(), fm.map().len());
    nc_daemon::daemon::run(&mut fm, &tx, &mut rx, &mut hooks).await;
    let _ = std::fs::remove_file(&socket);
    ExitCode::SUCCESS
}
