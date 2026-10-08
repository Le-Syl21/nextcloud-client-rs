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

use std::sync::{Arc, OnceLock};

use http_server::{HttpResponse, HttpServer, route};

const USER_JSON: &str = r#"{"ocs":{"meta":{"status":"ok","statuscode":100,"message":"OK"},"data":{"id":"alice","display-name":"Alice A."}}}"#;
const PROPFIND_207: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:"><d:response><d:href>/remote.php/dav/files/alice/</d:href><d:propstat><d:prop><d:getlastmodified>Mon, 05 Oct 2026 10:00:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;

/// `ocs/v1.php/cloud/user`, for alice with the app password `secret`.
fn user_route() -> http_server::Route {
    route(|r| {
        if r.headers.get("authorization").map(String::as_str) == Some("Basic YWxpY2U6c2VjcmV0") {
            HttpResponse::ok(&[("Content-Type", "application/json")], USER_JSON)
        } else {
            HttpResponse::with_status(401, &[], "")
        }
    })
}

/// The PROPFIND of alice's DAV root, answered with `propfind_status`.
fn propfind_route(propfind_status: u16) -> http_server::Route {
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
    })
}

fn fake_server(propfind_status: u16) -> HttpServer {
    HttpServer::start(vec![
        ("/ocs/v1.php/cloud/user", user_route()),
        (
            "/remote.php/dav/files/alice/",
            propfind_route(propfind_status),
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

/// A server with Login Flow v2: the browser logs in as `login_name` with
/// the app password `secret`, then the routes of [`fake_server`].
fn login_flow_server(login_name: &'static str) -> HttpServer {
    let base: Arc<OnceLock<String>> = Arc::default();
    let (b1, b2) = (base.clone(), base.clone());
    let server = HttpServer::start(vec![
        (
            "/status.php",
            route(|_| {
                HttpResponse::ok(
                    &[("Content-Type", "application/json")],
                    r#"{"installed":true,"maintenance":false,"needsDbUpgrade":false,"version":"30.0.0.1","versionstring":"30.0.0","edition":"","productname":"Nextcloud","extendedSupport":false}"#,
                )
            }),
        ),
        (
            "/index.php/login/v2",
            route(move |_| {
                let base = b1.get().unwrap();
                HttpResponse::ok(
                    &[("Content-Type", "application/json")],
                    format!(
                        r#"{{"poll":{{"token":"tok","endpoint":"{base}/login/v2/poll"}},"login":"{base}/login/v2/flow/abc"}}"#
                    ),
                )
            }),
        ),
        (
            "/login/v2/poll",
            route(move |_| {
                let base = b2.get().unwrap();
                HttpResponse::ok(
                    &[("Content-Type", "application/json")],
                    format!(
                        r#"{{"server":"{base}","loginName":"{login_name}","appPassword":"secret"}}"#
                    ),
                )
            }),
        ),
        ("/ocs/v1.php/cloud/user", user_route()),
        ("/remote.php/dav/files/alice/", propfind_route(207)),
    ]);
    base.set(server.base_url()).unwrap();
    server
}

/// The paths the server was asked for, in order.
fn requested_paths(server: &HttpServer) -> Vec<String> {
    server.requests().into_iter().map(|r| r.path).collect()
}

// Divergence: without --apppassword upstream stores the account without
// credentials (for the GUI to log in later); here the login is done right
// away with Login Flow v2, its link printed in the terminal.
#[test]
fn rust_only_provisioning_without_app_password_logs_in_with_login_flow_v2() {
    let sb = Sandbox::new();
    let server = login_flow_server("alice");
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        &server.base_url(),
        "--localdirpath",
        local.to_str().unwrap(),
    ]);
    assert_eq!(exit_code, 0, "{output}");
    // The link, with the login name prefilled (server 24 or later).
    assert!(
        output.contains(&format!(
            "{}/login/v2/flow/abc?user=alice",
            server.base_url()
        )),
        "{output}"
    );
    assert!(
        output.contains("setup from command line success."),
        "{output}"
    );
    let paths = requested_paths(&server);
    let login = paths.iter().position(|p| p == "/index.php/login/v2");
    let poll = paths.iter().position(|p| p == "/login/v2/poll");
    let user = paths.iter().position(|p| p == "/ocs/v1.php/cloud/user");
    assert!(login < poll && poll < user && login.is_some(), "{paths:?}");
    let cfg = std::fs::read_to_string(conf_dir.join("ncsyncd.cfg")).unwrap();
    assert!(cfg.contains("0\\webflow_user=alice"), "{cfg}");
    assert!(cfg.contains("0\\dav_user=alice"), "{cfg}");
    assert!(!cfg.contains("secret"), "{cfg}");
    let password_file = sb
        .path()
        .join("home/.local/state/ncsyncd/credentials/ncsyncd-0");
    assert_eq!(
        std::fs::read_to_string(&password_file).unwrap().trim(),
        "secret"
    );
    assert!(local.is_dir());
}

// With --non-interactive nobody can open the login link: without an app
// password the setup is refused before anything is sent, with an error
// that names --apppassword.
#[test]
fn rust_only_provisioning_non_interactive_without_app_password_is_refused() {
    let sb = Sandbox::new();
    let server = login_flow_server("alice");
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    let (output, exit_code) = sb.run_cmd(&[
        "--non-interactive",
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        &server.base_url(),
        "--localdirpath",
        local.to_str().unwrap(),
    ]);
    assert_eq!(exit_code, REJECTED, "{output}");
    assert!(output.contains("No app password"), "{output}");
    assert!(output.contains("--apppassword"), "{output}");
    assert!(!output.contains("login/v2/flow"), "{output}");
    assert!(requested_paths(&server).is_empty());
    assert!(!config_has_account(&conf_dir));
    assert!(!local.exists());
}

// The browser must log in as the --userid: another login name is a
// failure and nothing is stored.
#[test]
fn rust_only_provisioning_login_flow_as_another_user_writes_nothing() {
    let sb = Sandbox::new();
    let server = login_flow_server("bob");
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        &server.base_url(),
        "--localdirpath",
        local.to_str().unwrap(),
    ]);
    assert_eq!(exit_code, 1, "{output}");
    assert!(
        output.contains("the browser logged in as bob, not as alice"),
        "{output}"
    );
    assert!(!requested_paths(&server).contains(&"/ocs/v1.php/cloud/user".to_owned()));
    assert!(!config_has_account(&conf_dir));
    assert!(!local.exists());
}

// Login Flow v2 against an unreachable server: a failure, nothing stored.
#[test]
fn rust_only_provisioning_login_flow_unreachable_server_writes_nothing() {
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    let (output, exit_code) = sb.run_cmd(&[
        "--confdir",
        conf_dir.to_str().unwrap(),
        "--userid",
        "alice",
        "--serverurl",
        unreachable_server_url(),
    ]);
    assert_eq!(exit_code, 1, "{output}");
    assert!(
        output.contains("setup from command line failed with error"),
        "{output}"
    );
    assert!(!config_has_account(&conf_dir));
}

/// [`fake_server`] behind TLS with a self-signed certificate.
fn tls_fake_server() -> HttpServer {
    use tokio_rustls::rustls;
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key)
            .unwrap(),
    );
    HttpServer::start_wrapped(
        vec![
            ("/ocs/v1.php/cloud/user", user_route()),
            ("/remote.php/dav/files/alice/", propfind_route(207)),
        ],
        "https",
        Box::new(move |tcp| {
            let conn = rustls::ServerConnection::new(config.clone()).ok()?;
            Some(Box::new(rustls::StreamOwned::new(conn, tcp)))
        }),
    )
}

// Divergence: --trust applies to the setup's requests (upstream ignores it
// in provisioning mode), so a server with a self-signed certificate can be
// provisioned.
#[test]
fn rust_only_provisioning_trust_accepts_an_invalid_certificate() {
    let server = tls_fake_server();
    let args = |conf_dir: &Path, local: &Path| {
        vec![
            "--confdir".to_owned(),
            conf_dir.to_str().unwrap().to_owned(),
            "--userid".to_owned(),
            "alice".to_owned(),
            "--apppassword".to_owned(),
            "secret".to_owned(),
            "--serverurl".to_owned(),
            server.base_url(),
            "--localdirpath".to_owned(),
            local.to_str().unwrap().to_owned(),
        ]
    };

    // Without --trust the certificate is refused and nothing is stored.
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    let a = args(&conf_dir, &local);
    let (output, exit_code) = sb.run_cmd(&a.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(exit_code, 1, "{output}");
    assert!(output.contains("Could not fetch username."), "{output}");
    assert!(!config_has_account(&conf_dir));

    // With --trust the setup goes through.
    let sb = Sandbox::new();
    let conf_dir = sb.path().join("conf");
    let local = sb.path().join("sync");
    let mut a = args(&conf_dir, &local);
    a.insert(0, "--trust".to_owned());
    let (output, exit_code) = sb.run_cmd(&a.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(exit_code, 0, "{output}");
    assert!(
        output.contains("setup from command line success."),
        "{output}"
    );
    assert!(config_has_account(&conf_dir));
    let cfg = std::fs::read_to_string(conf_dir.join("ncsyncd.cfg")).unwrap();
    assert!(
        cfg.contains(&format!("0\\url={}", server.base_url())),
        "{cfg}"
    );
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
