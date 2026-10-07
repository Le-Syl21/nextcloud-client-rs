// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `ncsync status|pause|resume|sync-now`: requests to a running `ncsyncd`
//! over its control socket. The answer is printed as JSON with `--json`,
//! as a short summary otherwise.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use nc_daemon::control::{Request, default_socket_path, request};

/// Which daemon to talk to.
#[derive(Args, Debug, Clone)]
pub struct ControlArgs {
    /// The daemon's control socket (default $XDG_RUNTIME_DIR/ncsyncd/control.sock,
    /// or /run/ncsyncd/NAME/control.sock with --instance).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Talk to the system instance ncsyncd@NAME.
    #[arg(long, value_name = "NAME")]
    instance: Option<String>,
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

fn send(control: &ControlArgs, req: &Request) -> Result<serde_json::Value, ExitCode> {
    let path = control
        .socket
        .clone()
        .unwrap_or_else(|| default_socket_path(control.instance.as_deref()));
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
