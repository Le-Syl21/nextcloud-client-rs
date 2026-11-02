// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `ncsync status|pause|resume|sync-now|events`: requests to a running `ncsyncd`
//! over its control socket. The answer is printed as JSON with `--json`,
//! as a short summary otherwise.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use nc_daemon::config_file::ConfigLocation;
use nc_daemon::control::{Request, default_socket_path, request};

/// Which daemon to talk to.
#[derive(Args, Debug, Clone)]
pub struct ControlArgs {
    /// The daemon's control socket (default: the one of the daemon using the
    /// configuration, $XDG_RUNTIME_DIR/ncsyncd/HASH.sock, or
    /// /run/ncsyncd/NAME/HASH.sock with --instance).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Talk to the system instance ncsyncd@NAME.
    #[arg(long, value_name = "NAME")]
    instance: Option<String>,
    /// The daemon's configuration file, which names its socket (default
    /// ~/.config/ncsyncd/ncsyncd.cfg, or /var/lib/ncsyncd/NAME/ncsyncd.cfg
    /// with --instance).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Print the daemon's JSON answer.
    #[arg(long)]
    json: bool,
}

/// A folder argument.
#[derive(Args, Debug, Clone)]
pub struct FolderArg {
    /// The folder's alias or local path (default: all folders).
    folder: Option<String>,
    #[command(flatten)]
    control: ControlArgs,
}

/// The socket to talk to.
pub fn socket_of(control: &ControlArgs) -> Result<PathBuf, ExitCode> {
    if let Some(s) = &control.socket {
        return Ok(s.clone());
    }
    let location = match &control.instance {
        Some(i) => ConfigLocation::system(i),
        None => ConfigLocation::user(),
    };
    match location {
        Ok(l) => Ok(default_socket_path(&match &control.config {
            Some(c) => l.with_config_file(c),
            None => l,
        })),
        Err(e) => {
            eprintln!("{e}");
            Err(ExitCode::FAILURE)
        }
    }
}

fn send(control: &ControlArgs, req: &Request) -> Result<serde_json::Value, ExitCode> {
    let path = socket_of(control)?;
    match request(&path, req) {
        Ok(v) => Ok(v),
        Err(e) => {
            eprintln!(
                "Cannot reach ncsyncd at {}: {e} (is the daemon running?)",
                path.display()
            );
            Err(ExitCode::FAILURE)
        }
    }
}

fn finish(
    control: &ControlArgs,
    v: &serde_json::Value,
    text: impl FnOnce(&serde_json::Value),
) -> ExitCode {
    if control.json {
        println!("{v}");
    } else if v["ok"] == true {
        text(v);
    }
    if v["ok"] == true {
        ExitCode::SUCCESS
    } else {
        if !control.json {
            eprintln!("ncsyncd: {}", v["error"].as_str().unwrap_or("error"));
        }
        ExitCode::FAILURE
    }
}

/// `ncsync status`.
pub fn run_status(control: ControlArgs) -> ExitCode {
    let v = match send(&control, &Request::Status) {
        Ok(v) => v,
        Err(c) => return c,
    };
    finish(&control, &v, |v| {
        for a in v["accounts"].as_array().into_iter().flatten() {
            println!(
                "account {} {} ({}): {}",
                a["id"].as_str().unwrap_or(""),
                a["user"].as_str().unwrap_or(""),
                a["url"].as_str().unwrap_or(""),
                a["state"].as_str().unwrap_or("")
            );
            for e in a["errors"].as_array().into_iter().flatten() {
                println!("  {}", e.as_str().unwrap_or(""));
            }
        }
        for f in v["folders"].as_array().into_iter().flatten() {
            let mut flags = Vec::new();
            for (k, label) in [
                ("paused", "paused"),
                ("syncing", "syncing"),
                ("scheduled", "scheduled"),
            ] {
                if f[k] == true {
                    flags.push(label);
                }
            }
            println!(
                "folder {} {} <-> {}: {}{}",
                f["alias"].as_str().unwrap_or(""),
                f["local_path"].as_str().unwrap_or(""),
                f["remote_path"].as_str().unwrap_or(""),
                f["status_string"].as_str().unwrap_or(""),
                if flags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", flags.join(", "))
                }
            );
            for e in f["errors"].as_array().into_iter().flatten() {
                println!("  {}", e.as_str().unwrap_or(""));
            }
        }
    })
}

/// `ncsync pause|resume|sync-now [FOLDER]`.
pub fn run_folder_request(
    arg: FolderArg,
    make: fn(Option<String>) -> Request,
    done: &str,
) -> ExitCode {
    let req = make(arg.folder.clone());
    let v = match send(&arg.control, &req) {
        Ok(v) => v,
        Err(c) => return c,
    };
    finish(&arg.control, &v, |v| {
        let folders: Vec<&str> = v["folders"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str())
            .collect();
        println!("{done}: {}", folders.join(", "));
    })
}

/// `ncsync events`: subscribes to the daemon's event stream and prints it
/// until the daemon stops (or Ctrl+C).
pub fn run_events(control: ControlArgs) -> ExitCode {
    use nc_daemon::client::Remote;
    use std::io::Write;
    let path = match socket_of(&control) {
        Ok(p) => p,
        Err(c) => return c,
    };
    let mut sub = match Remote::new(&path).try_subscribe() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Cannot reach ncsyncd at {}: {e} (is the daemon running?)",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout().lock();
    while let Some(event) = sub.blocking_recv() {
        let Ok(text) = serde_json::to_string(&event) else {
            continue;
        };
        if writeln!(out, "{text}").and_then(|()| out.flush()).is_err() {
            return ExitCode::SUCCESS;
        }
    }
    eprintln!("ncsyncd stopped");
    ExitCode::SUCCESS
}
