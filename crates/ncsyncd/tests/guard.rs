// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! rust_only: the instance guard across processes. A second `ncsyncd` on
//! the same configuration is refused and names the first; once the first
//! is killed with SIGKILL, the OS has released its lock and a new one
//! starts.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nc_daemon::account_config::{AccountDefinition, save_account};
use nc_daemon::settings::Settings;

fn ncsyncd(dir: &Path, cfg: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ncsyncd"));
    // The same endpoint as the default one on Unix; a named pipe on Windows
    // is named after this path.
    let socket = dir
        .join("run/ncsyncd")
        .join(format!("{}.sock", nc_daemon::guard::config_hash(cfg)));
    c.arg("--config")
        .arg(cfg)
        .arg("--socket")
        .arg(socket)
        .args(["--no-watch", "--no-push", "--official-config"])
        .arg(dir.join("no-official.cfg"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        // No keyring: the account has a password file.
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env_remove("NOTIFY_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c
}

/// Waits until the daemon answers on its socket.
fn wait_listening(socket: &Path, child: &mut Child) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if nc_daemon::transport::connect(socket).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("ncsyncd exited with {status}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("ncsyncd does not listen on {}", socket.display());
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn rust_only_guard_refuses_a_second_daemon_until_the_first_dies() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("ncsyncd.cfg");
    let pw = dir.path().join("pw");
    std::fs::write(&pw, "secret\n").unwrap();
    // An account on a closed port: the daemon keeps trying to connect.
    let mut acc = AccountDefinition::new_webflow("0", "http://127.0.0.1:9", "alice");
    acc.password_file = Some(pw.to_string_lossy().into_owned());
    Settings::modify(&cfg, |s| save_account(s, &acc)).unwrap();
    let socket: PathBuf = dir
        .path()
        .join("run/ncsyncd")
        .join(format!("{}.sock", nc_daemon::guard::config_hash(&cfg)));

    let mut first = ncsyncd(dir.path(), &cfg).spawn().unwrap();
    wait_listening(&socket, &mut first);

    // The second one is refused, and says who holds the configuration.
    let second = ncsyncd(dir.path(), &cfg).spawn().unwrap();
    let out = second.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the second ncsyncd was not refused");
    assert!(
        stderr.contains(&format!(
            "is already in use by ncsyncd (pid {})",
            first.id()
        )),
        "{stderr}"
    );

    // kill -9: no clean-up ran, the socket file is stale, but the lock is
    // free again.
    kill(&mut first);
    #[cfg(unix)]
    assert!(socket.exists());
    let mut third = ncsyncd(dir.path(), &cfg).spawn().unwrap();
    wait_listening(&socket, &mut third);
    kill(&mut third);
}
