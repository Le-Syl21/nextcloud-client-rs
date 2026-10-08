// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: CC0-1.0
//
// Port of upstream test/testnextcloudcmdprovisioning.cpp (nextcloud/desktop
// v34.0.5). `nextcloudcmd ARGS` is `ncsync sync ARGS`.

//! The account provisioning mode of `ncsync sync` (`--userid`, ...), run as
//! a child process like upstream's `QProcess`.
//!
//! Every run gets its own `HOME` and XDG directories (upstream's tests
//! without `--confdir` use the real configuration of the user running
//! them) and no D-Bus session, so that nothing reaches the user's
//! configuration or keyring.

#[path = "../../nc-testutils/tests/common/http_server.rs"]
mod http_server;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A sandbox for one `ncsync` run.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `runCmd(args, &exitCode, timeoutMs)`: combined stdout and stderr and
    /// the exit code; stdin is closed so that an accidental interactive
    /// read gets EOF rather than blocking the test suite.
    fn run_cmd(&self, args: &[&str]) -> (String, i32) {
        self.run_cmd_with_timeout(args, Duration::from_millis(12000))
    }

    fn run_cmd_with_timeout(&self, args: &[&str], timeout: Duration) -> (String, i32) {
        let home = self.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let out = self.path().join("output");
        let file = std::fs::File::create(&out).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ncsync"))
            .arg("sync")
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_RUNTIME_DIR", self.path())
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus")
            .env_remove("CREDENTIALS_DIRECTORY")
            .env_remove("NC_USER")
            .env_remove("NC_PASSWORD")
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if started.elapsed() > timeout {
                // Kill rather than wait on a wedged child, and leave an exit code
                // behind that no expectation below accepts.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let exit_code = status.and_then(|s| s.code()).unwrap_or(-1000);
        (std::fs::read_to_string(&out).unwrap(), exit_code)
    }

    /// The configuration of a run without `--confdir`.
    fn default_conf_dir(&self) -> std::path::PathBuf {
        self.path().join("home/.config/ncsyncd")
    }
}

/// `configHasAccount(confDir)`: an account section was written to a
/// configuration in `conf_dir`.
fn config_has_account(conf_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(conf_dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|e| {
        e.path().extension().is_some_and(|x| x == "cfg")
            && std::fs::read_to_string(e.path()).is_ok_and(|c| c.contains("webflow_user"))
    })
}

/// `unreachableServerUrl()`: a port nothing listens on, so the setup fails
/// on the first request.
fn unreachable_server_url() -> &'static str {
    "http://127.0.0.1:1"
}

/// The exit code of upstream's `return -1` (`expectedExitCode` on Linux).
const REJECTED: i32 = 255;

// --userid without --serverurl must print a descriptive error and exit 255.
// It must NOT print the interactive "Please enter username:" prompt.
#[test]
fn test_user_id_alone_prints_server_url_error() {
    let sb = Sandbox::new();
    let (output, exit_code) = sb.run_cmd(&["--userid", "alice"]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(output.contains("--serverurl"), "{output}");
    assert!(!output.contains("Please enter username"), "{output}");
    assert!(!config_has_account(&sb.default_conf_dir()));
}

// --userid + --serverurl without --apppassword must enter provisioning mode
// (app password is optional). No credential prompt must appear.
#[test]
fn test_user_id_and_server_url_without_app_password_enters_provision_mode() {
    let sb = Sandbox::new();
    // The quotes are part of the value: an invalid URL.
    let (output, exit_code) =
        sb.run_cmd(&["--userid", "alice", "--serverurl", "\"http://127.0.0.1:1\""]);
    assert_eq!(exit_code, REJECTED, "{output}");
    // Must NOT fall through into interactive sync mode.
    assert!(!output.contains("Please enter username"), "{output}");
    assert!(
        !output.contains("Password for account with username"),
        "{output}"
    );
}

// --userid + --serverurl (+ optional --apppassword) pointing at an invalid
// server URL: provisioning mode (no credential prompt), and an error.
#[test]
fn test_provisioning_options_enter_provision_mode_not_sync_mode() {
    let sb = Sandbox::new();
    let (output, exit_code) = sb.run_cmd(&[
        "--userid",
        "alice",
        "--apppassword",
        "secret",
        "--serverurl",
        "\"http://127.0.0.1:1\"",
    ]);
    assert_eq!(exit_code, REJECTED, "{output}");
    // Provisioning mode must NOT fall through into interactive sync mode.
    assert!(!output.contains("Please enter username"), "{output}");
    assert!(
        !output.contains("Password for account with username"),
        "{output}"
    );
}

// No arguments: help text is printed, exit 0. Guards against regressions in
// the normal (non-provisioning) sync-mode entry path.
#[test]
fn test_no_args_shows_help() {
    let sb = Sandbox::new();
    let (output, exit_code) = sb.run_cmd(&[]);
    assert_eq!(exit_code, 0, "{output}");
    assert!(output.contains("ncsync"), "{output}");
    assert!(output.contains("--userid"), "{output}");
}

// --non-interactive combined with --userid (but missing the other two
// provisioning options) must still print a structured error, not a prompt.
#[test]
fn test_non_interactive_flag_does_not_suppress_provisioning_error() {
    let sb = Sandbox::new();
    let (output, exit_code) = sb.run_cmd(&["--non-interactive", "--userid", "alice"]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(output.contains("--serverurl"), "{output}");
    assert!(!output.contains("Please enter username"), "{output}");
}

// The same as test_user_id_alone_prints_server_url_error, with the inline
// "--option=value" spelling.
#[test]
fn test_inline_option_values_select_provisioning_mode() {
    let sb = Sandbox::new();
    let (output, exit_code) = sb.run_cmd(&["--userid=alice"]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(output.contains("--serverurl"), "{output}");
    assert!(!output.contains("Please enter username"), "{output}");
}

// A full set of provisioning options in the inline spelling has to reach
// the account setup. Sync mode would instead have taken the last two
// arguments as the positional source directory and server URL.
#[test]
fn test_inline_option_values_reach_account_setup() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    std::fs::create_dir_all(&conf_dir).unwrap();
    let server = format!("--serverurl={}", unreachable_server_url());
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid=alice",
        "--apppassword=secret",
        &server,
    ]);
    assert!(output.contains("Could not fetch username"), "{output}");
    assert!(!output.contains("does not exist"), "{output}");
    assert_eq!(exit_code, 1, "{output}");
}

// --localdirpath is optional: leaving it out must not abort the setup
// before the credentials are sent to the server.
#[test]
fn test_setup_without_local_dir_path_is_not_rejected() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    std::fs::create_dir_all(&conf_dir).unwrap();
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--apppassword",
        "secret",
        "--serverurl",
        unreachable_server_url(),
    ]);
    assert!(
        !output.contains("local folder because the name is empty"),
        "{output}"
    );
    // The setup got as far as contacting the server, which is where it fails here.
    assert!(output.contains("Could not fetch username"), "{output}");
    assert_eq!(exit_code, 1, "{output}");
}

// With an app password the credentials are checked against the server
// before the account is written: an unreachable server is a failure (1),
// and nothing is left in the configuration.
#[test]
fn test_app_password_setup_runs_the_event_loop() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    std::fs::create_dir_all(&conf_dir).unwrap();
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--apppassword",
        "secret",
        "--serverurl",
        unreachable_server_url(),
    ]);
    // 1 is the code the setup job exits with: neither the 255 of a rejected
    // command line nor the 0 of an early return.
    assert_eq!(exit_code, 1, "{output}");
    assert!(output.contains("Could not fetch username"), "{output}");
    assert!(
        !config_has_account(&conf_dir),
        "a failed setup must not store an account"
    );
}

// ---------------------------------------------------------------------------
// rust_only
// ---------------------------------------------------------------------------

use http_server::{HttpResponse, HttpServer, route};

const USER_JSON: &str = r#"{"ocs":{"meta":{"status":"ok","statuscode":100,"message":"OK"},"data":{"id":"alice","display-name":"Alice A."}}}"#;
const PROPFIND_207: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:"><d:response><d:href>/remote.php/dav/files/alice/</d:href><d:propstat><d:prop><d:getlastmodified>Mon, 05 Oct 2026 10:00:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;

fn fake_server(propfind_status: u16) -> HttpServer {
    HttpServer::start(vec![
        (
            "/ocs/v1.php/cloud/user",
            route(|r| {
                if r.headers.get("authorization").map(String::as_str)
                    == Some("Basic YWxpY2U6c2VjcmV0")
                {
                    HttpResponse::ok(&[("Content-Type", "application/json")], USER_JSON)
                } else {
                    HttpResponse::with_status(401, &[], "")
                }
            }),
        ),
        (
            "/remote.php/dav/files/alice/",
            route(move |_| {
                HttpResponse::with_status(
                    propfind_status,
                    &[("Content-Type", "application/xml; charset=utf-8")],
                    if propfind_status == 207 {
                        PROPFIND_207
                    } else {
                        ""
                    },
                )
            }),
        ),
    ])
}

#[test]
fn rust_only_provisioning_writes_account_folder_and_app_password() {
    let sb = Sandbox::new();
    let server = fake_server(207);
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync/Alice");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--apppassword",
        "secret",
        "--serverurl",
        &server.base_url(),
        "--localdirpath",
        local.to_str().unwrap(),
        "--remotedirpath",
        "Photos/",
    ]);
    assert_eq!(exit_code, 0, "{output}");
    let port = server.base_url().rsplit(':').next().unwrap().to_owned();
    assert!(
        output.contains(&format!(
            "Account alice@127.0.0.1:{port} setup from command line success."
        )),
        "{output}"
    );
    assert!(config_has_account(&conf_dir));
    let cfg = std::fs::read_to_string(conf_dir.join("ncsyncd.cfg")).unwrap();
    assert!(cfg.contains("0\\webflow_user=alice"), "{cfg}");
    assert!(cfg.contains("0\\dav_user=alice"), "{cfg}");
    assert!(cfg.contains("0\\displayName=Alice A."), "{cfg}");
    assert!(cfg.contains("0\\Folders\\0\\targetPath=/Photos"), "{cfg}");
    assert!(cfg.contains("0\\Folders\\0\\virtualFilesMode=off"), "{cfg}");
    assert!(
        !cfg.contains("secret"),
        "the app password is not in the configuration"
    );
    // No keyring here: the app password went to a password file.
    let password_file = sb
        .path()
        .join("home/.local/state/ncsyncd/credentials/ncsyncd-0");
    assert_eq!(
        std::fs::read_to_string(&password_file).unwrap().trim(),
        "secret"
    );
    assert!(local.is_dir());
    // The journal has the "/" whitelist of setupLocalSyncFolder.
    let journal = std::fs::read_dir(&local)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with(".sync_"));
    assert!(journal.is_some(), "the folder's journal was created");

    // The same account again is refused before anything is sent.
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--apppassword",
        "secret",
        "--serverurl",
        &server.base_url(),
    ]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(output.contains("Account alice already exists!"), "{output}");
}

#[test]
fn rust_only_provisioning_refuses_virtual_files_and_writes_nothing() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        "http://127.0.0.1:1",
        "--isvfsenabled",
        "1",
    ]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(
        output.contains("Virtual files are not supported"),
        "{output}"
    );
    assert!(!config_has_account(&conf_dir));
}

#[test]
fn rust_only_provisioning_wrong_app_password_writes_nothing() {
    let sb = Sandbox::new();
    let server = fake_server(207);
    let conf_dir = sb.path().join("conf");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--apppassword",
        "wrong",
        "--serverurl",
        &server.base_url(),
        "--localdirpath",
        sb.path().join("sync").to_str().unwrap(),
    ]);
    assert_eq!(exit_code, 1, "{output}");
    assert!(output.contains("Could not fetch username."), "{output}");
    assert!(!config_has_account(&conf_dir));
    assert!(!sb.path().join("sync").exists());
}

#[test]
fn rust_only_provisioning_without_app_password_stores_the_account_only() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        "http://127.0.0.1:1",
        "--localdirpath",
        local.to_str().unwrap(),
    ]);
    // Nothing is sent to the server: the account is stored for a later login.
    assert_eq!(exit_code, 0, "{output}");
    assert!(output.contains("ncsync account add"), "{output}");
    let cfg = std::fs::read_to_string(conf_dir.join("ncsyncd.cfg")).unwrap();
    assert!(cfg.contains("0\\dav_user=alice"), "{cfg}");
    assert!(!cfg.contains("ncsyncd_passwordFile"), "{cfg}");
    assert!(local.is_dir());
}

#[test]
fn rust_only_provisioning_non_empty_local_folder_is_rejected() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("file"), b"x").unwrap();
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        "http://127.0.0.1:1",
        "--localdirpath",
        local.to_str().unwrap(),
    ]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(
        output.contains("already exists and is non-empty!"),
        "{output}"
    );
    assert!(!config_has_account(&conf_dir));
}

#[test]
fn rust_only_provisioning_unknown_option_shows_help() {
    let sb = Sandbox::new();
    // parseOptions: an option nobody knows prints the help (HelpMode, 0).
    let (output, exit_code) = sb.run_cmd(&["--userid", "alice", "--bogus"]);
    assert_eq!(exit_code, 0, "{output}");
    assert!(output.contains("--userid"), "{output}");
    // An option without its value too ("userid not specified").
    let (output, exit_code) = sb.run_cmd(&["--userid"]);
    assert_eq!(exit_code, 0, "{output}");
    assert!(output.contains("--serverurl"), "{output}");
}
