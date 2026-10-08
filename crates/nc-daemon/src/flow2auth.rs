// SPDX-FileCopyrightText: 2019 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/creds/flow2auth.cpp` (nextcloud/desktop v34.0.5),
// for a terminal: the login link is printed (with a QR code) instead of
// being opened in a browser.

//! Login Flow v2: `POST index.php/login/v2` gives a login link and a poll
//! endpoint with a token; the poll endpoint answers 404 until the user has
//! granted access in the browser, then the server URL, the login name and a
//! new app password.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use http::{HeaderMap, HeaderValue, Method};
use nc_dav::{Account, JobOptions, NetworkError, Reply, ServerUrl, Target, Transport};
use serde_json::{Map, Value};

use crate::credentials::AppPassword;

/// `loginFlowPollIntervalSeconds`.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// How long [`wait_for_login`] polls by default: the server forgets a login
/// flow after 20 minutes.
pub const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// `usernamePrefillServerVersionMinSupportedMajor`.
const USERNAME_PREFILL_MIN_MAJOR: i32 = 24;
const LOG_TARGET: &str = "nextcloud.sync.credentials.flow2auth";

/// Errors of the login flow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Flow2Error {
    /// The server answered with an error (message as upstream shows it).
    #[error("{0}")]
    Server(String),
    /// The login URL was HTTPS but the server or poll endpoint is not.
    #[error(
        "The returned server URL does not start with HTTPS despite the login URL started with HTTPS. Login will not be possible because this might be a security issue. Please contact your administrator."
    )]
    NotHttps,
    #[error(
        "The server did not reply with the expected data. Please try connecting again later or contact your server administrator if the issue continues."
    )]
    UnexpectedData,
    #[error("the login was not completed within {0:?}")]
    Timeout(Duration),
    #[error("invalid URL {0}")]
    InvalidUrl(String),
}

/// A started login flow.
#[derive(Clone)]
pub struct LoginFlow {
    /// The link the user opens in a browser.
    pub login_url: String,
    poll_token: String,
    pub poll_endpoint: String,
    enforce_https: bool,
}

impl std::fmt::Debug for LoginFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginFlow")
            .field("login_url", &self.login_url)
            .field("poll_endpoint", &self.poll_endpoint)
            .finish_non_exhaustive()
    }
}

/// A successful login.
#[derive(Clone, Debug)]
pub struct LoginResult {
    pub server_url: String,
    pub login_name: String,
    pub app_password: AppPassword,
}

/// What a poll gave.
enum Handled {
    Json(Map<String, Value>),
    /// 404: the user has not finished yet.
    NotFound,
}

/// `Flow2Auth::handleResponse`.
fn handle_response(reply: &Reply, enforce_https: bool) -> Result<Handled, Flow2Error> {
    let parsed: Result<Value, _> = serde_json::from_slice(&reply.body);
    let parse_ok = parsed.is_ok();
    let json = match parsed {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let str_of = |m: &Map<String, Value>, k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    if reply.error == NetworkError::NoError && parse_ok && !json.is_empty() {
        let server = str_of(&json, "server");
        let endpoint = if server.is_empty() {
            // from login/v2 endpoint
            json.get("poll")
                .and_then(|p| p.get("endpoint"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        } else {
            // from login/v2/poll endpoint
            server
        };
        log::debug!(target: LOG_TARGET, "Server url returned is {endpoint}");
        let is_https = !endpoint.is_empty() && endpoint.starts_with("https://");
        if enforce_https && !is_https {
            log::warn!(target: LOG_TARGET, "Returned server url | poll endpoint does not start with https");
            return Err(Flow2Error::NotHttps);
        }
    }
    if reply.error != NetworkError::NoError || !parse_ok {
        // We get a 404 until authentication is done. Upstream logs it as a warning, which
        // only reaches its log file; a terminal would show one pair every 3 s while the
        // user logs in, so it is a debug line here.
        if reply.error == NetworkError::ContentNotFoundError {
            log::debug!(target: LOG_TARGET, "Login not completed yet: {} - http status code: {}", reply.url, reply.http_status);
            return Ok(Handled::NotFound);
        }
        let reason = if reply.http_status == 503 {
            "The server is temporarily unavailable because it is in maintenance mode. Please try again once maintenance has finished.".to_owned()
        } else if !str_of(&json, "error").is_empty() {
            log::warn!(target: LOG_TARGET, "Error returned from JSON: {}", str_of(&json, "error"));
            "An unexpected error occurred when trying to access the server. Please try to access it again later or contact your server administrator if the issue continues.".to_owned()
        } else if reply.error != NetworkError::NoError {
            log::warn!(target: LOG_TARGET, "Error string returned from the server: {}", reply.error_string());
            "An unexpected error occurred when trying to access the server. Please try to access it again later or contact your server administrator if the issue continues.".to_owned()
        } else {
            // Could not parse the JSON returned from the server
            "We couldn't parse the server response. Please try connecting again later or contact your server administrator if the issue continues.".to_owned()
        };
        log::warn!(target: LOG_TARGET, "Error when requesting: {} - http status code: {}", reply.url, reply.http_status);
        return Err(Flow2Error::Server(reason));
    }
    Ok(Handled::Json(json))
}

fn short_timeout() -> JobOptions {
    let mut opts = JobOptions::default();
    opts.timeout = opts.timeout.min(Duration::from_secs(30));
    opts
}

/// Step 1 (`fetchNewToken`): an anonymous `POST index.php/login/v2`.
/// `prefill_user` is added to the login link (`?user=`) when the server
/// supports it (server version 24 and later, as known by `account`).
pub async fn start_login(
    account: &Account,
    prefill_user: Option<&str>,
) -> Result<LoginFlow, Flow2Error> {
    let enforce_https = account.url().scheme == "https";
    // add 'Content-Length: 0' header (see https://github.com/nextcloud/desktop/issues/1473)
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    let reply = nc_dav::jobs::send(
        account,
        Method::POST,
        &Target::Account("/index.php/login/v2".to_owned()),
        headers,
        nc_dav::Body::empty(),
        &short_timeout(),
    )
    .await;
    let json = match handle_response(&reply, enforce_https)? {
        Handled::Json(j) => j,
        Handled::NotFound => {
            return Err(Flow2Error::Server(format!(
                "Login Flow v2 is not available on this server ({})",
                reply.url
            )));
        }
    };
    let poll = json.get("poll");
    let get = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_owned();
    let poll_token = get(poll.and_then(|p| p.get("token")));
    let poll_endpoint = get(poll.and_then(|p| p.get("endpoint")));
    let mut login_url = get(json.get("login"));
    if poll_token.is_empty() || poll_endpoint.is_empty() || login_url.is_empty() {
        return Err(Flow2Error::UnexpectedData);
    }
    if let Some(user) = prefill_user.filter(|u| !u.is_empty())
        && account.server_version_int()
            >= nc_dav::account::make_server_version(USERNAME_PREFILL_MIN_MAJOR, 0, 0)
    {
        let item = nc_dav::account::encode_query_items(&[("user".to_owned(), user.to_owned())]);
        login_url.push(if login_url.contains('?') { '&' } else { '?' });
        login_url.push_str(&item);
    }
    log::info!(target: LOG_TARGET, "setting login flow poll timer interval to {} msec", POLL_INTERVAL.as_millis());
    Ok(LoginFlow {
        login_url,
        poll_token,
        poll_endpoint,
        enforce_https,
    })
}

/// Step 2 (`slotPollTimerTimeout`): one poll. `Ok(None)` while the user has
/// not granted access (or the answer was incomplete: poll again).
pub async fn poll_once(
    transport: &Arc<dyn Transport>,
    flow: &LoginFlow,
) -> Result<Option<LoginResult>, Flow2Error> {
    let endpoint = ServerUrl::parse(&flow.poll_endpoint)
        .map_err(|_| Flow2Error::InvalidUrl(flow.poll_endpoint.clone()))?;
    let path = endpoint.path.clone();
    let poll_account = Account::new(endpoint, transport.clone());
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let body =
        nc_dav::account::encode_query_items(&[("token".to_owned(), flow.poll_token.clone())]);
    let reply = nc_dav::jobs::send(
        &poll_account,
        Method::POST,
        &Target::Absolute(path),
        headers,
        nc_dav::Body::from(nc_dav::Bytes::from(body.into_bytes())),
        &short_timeout(),
    )
    .await;
    let json = match handle_response(&reply, flow.enforce_https)? {
        Handled::Json(j) => j,
        Handled::NotFound => return Ok(None),
    };
    let get = |k: &str| {
        json.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (server_url, login_name, app_password) =
        (get("server"), get("loginName"), get("appPassword"));
    if server_url.is_empty() || login_name.is_empty() || app_password.is_empty() {
        // Failed: poll again
        return Ok(None);
    }
    log::info!(target: LOG_TARGET, "Success getting the appPassword for user: {login_name}, server: {server_url}");
    Ok(Some(LoginResult {
        server_url,
        login_name,
        app_password: AppPassword::new(app_password),
    }))
}

/// Polls every [`POLL_INTERVAL`] until the login is done or `timeout`
/// passes. Server errors are reported to `on_error` and polling goes on,
/// like upstream (the GUI shows them and keeps its timer); a security error
/// (HTTPS downgrade) stops.
pub async fn wait_for_login(
    transport: &Arc<dyn Transport>,
    flow: &LoginFlow,
    timeout: Duration,
    mut on_error: impl FnMut(&Flow2Error),
) -> Result<LoginResult, Flow2Error> {
    let started = Instant::now();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match poll_once(transport, flow).await {
            Ok(Some(r)) => return Ok(r),
            Ok(None) => {}
            Err(Flow2Error::NotHttps) => return Err(Flow2Error::NotHttps),
            Err(e) => on_error(&e),
        }
        if started.elapsed() >= timeout {
            return Err(Flow2Error::Timeout(timeout));
        }
    }
}

/// The login link as a QR code for a terminal (Unicode half blocks, two
/// rows per line, light modules drawn so that it reads on a dark terminal).
pub fn render_qr(text: &str) -> Option<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    Some(
        code.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_dav::{Body, BoxFuture, Request, Response, TransportError};
    use std::sync::Mutex;

    /// A transport answering from a script of (method, path, status, body),
    /// recording the requests.
    /// (method, uri, content type, body) of a request.
    type Seen = (String, String, String, Vec<u8>);

    struct Scripted {
        replies: Mutex<Vec<(u16, &'static str)>>,
        seen: Mutex<Vec<Seen>>,
    }

    impl Transport for Scripted {
        fn send(&self, req: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
            Box::pin(async move {
                let (parts, body) = req.into_parts();
                let body = body.collect().await.unwrap_or_default().to_vec();
                let ct = parts
                    .headers
                    .get(http::header::CONTENT_TYPE)
                    .map(|v| v.to_str().unwrap_or_default().to_owned())
                    .unwrap_or_default();
                self.seen.lock().unwrap().push((
                    parts.method.to_string(),
                    parts.uri.to_string(),
                    ct,
                    body,
                ));
                let (status, text) = self.replies.lock().unwrap().remove(0);
                let mut resp = http::Response::new(Body::from(nc_dav::Bytes::from(text)));
                *resp.status_mut() = http::StatusCode::from_u16(status).unwrap();
                Ok(resp)
            })
        }
    }

    fn setup(
        url: &str,
        replies: Vec<(u16, &'static str)>,
    ) -> (Arc<Scripted>, Arc<dyn Transport>, Account) {
        let s = Arc::new(Scripted {
            replies: Mutex::new(replies),
            seen: Mutex::new(Vec::new()),
        });
        let t: Arc<dyn Transport> = s.clone();
        let account = Account::new(ServerUrl::parse(url).unwrap(), t.clone());
        (s, t, account)
    }

    const START: &str = r#"{"poll":{"token":"tok","endpoint":"https://cloud.example.com/login/v2/poll"},"login":"https://cloud.example.com/login/v2/flow/abc"}"#;

    #[tokio::test(start_paused = true)]
    async fn derived_login_flow_happy_path() {
        let (seen, t, account) = setup(
            "https://cloud.example.com",
            vec![
                (200, START),
                (404, "[]"),
                (
                    200,
                    r#"{"server":"https://cloud.example.com","loginName":"alice","appPassword":"app-pw"}"#,
                ),
            ],
        );
        account.set_server_version("30.0.0");
        let flow = start_login(&account, Some("alice")).await.unwrap();
        assert_eq!(
            flow.login_url,
            "https://cloud.example.com/login/v2/flow/abc?user=alice"
        );
        let mut errors = 0;
        let r = wait_for_login(&t, &flow, DEFAULT_LOGIN_TIMEOUT, |_| errors += 1)
            .await
            .unwrap();
        assert_eq!(errors, 0);
        assert_eq!(r.server_url, "https://cloud.example.com");
        assert_eq!(r.login_name, "alice");
        assert_eq!(r.app_password.expose(), "app-pw");
        let seen = seen.seen.lock().unwrap();
        assert_eq!(seen[0].0, "POST");
        assert_eq!(seen[0].1, "https://cloud.example.com/index.php/login/v2");
        assert_eq!(seen[1].1, "https://cloud.example.com/login/v2/poll");
        assert_eq!(seen[1].2, "application/x-www-form-urlencoded");
        assert_eq!(seen[1].3, b"token=tok");
        assert!(format!("{flow:?}").find("tok\"").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn derived_login_flow_errors() {
        // No prefill on an old server; an HTTP poll endpoint is refused.
        let (_, _, account) = setup(
            "https://cloud.example.com",
            vec![(
                200,
                r#"{"poll":{"token":"t","endpoint":"http://cloud.example.com/poll"},"login":"https://l"}"#,
            )],
        );
        assert_eq!(
            start_login(&account, Some("x")).await.unwrap_err(),
            Flow2Error::NotHttps
        );
        let (_, _, account) = setup("https://cloud.example.com", vec![(503, "")]);
        assert!(matches!(
            start_login(&account, None).await,
            Err(Flow2Error::Server(m)) if m.contains("maintenance")
        ));
        let (_, _, account) = setup("https://cloud.example.com", vec![(200, "{}")]);
        assert_eq!(
            start_login(&account, None).await.unwrap_err(),
            Flow2Error::UnexpectedData
        );
        // Poll errors are reported and polling continues, then the timeout.
        let (_, t, account) = setup(
            "https://cloud.example.com",
            vec![
                (200, START),
                (500, r#"{"error":"boom"}"#),
                (404, ""),
                (404, ""),
            ],
        );
        let flow = start_login(&account, None).await.unwrap();
        assert_eq!(
            flow.login_url,
            "https://cloud.example.com/login/v2/flow/abc"
        );
        let mut errors = Vec::new();
        let r = wait_for_login(&t, &flow, Duration::from_secs(8), |e| {
            errors.push(e.clone())
        })
        .await;
        assert_eq!(r.unwrap_err(), Flow2Error::Timeout(Duration::from_secs(8)));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("unexpected error"));
    }

    #[test]
    fn derived_qr_code_renders() {
        let qr = render_qr("https://cloud.example.com/login/v2/flow/abc").unwrap();
        assert!(qr.lines().count() > 10);
        assert!(qr.contains('█') || qr.contains('▀') || qr.contains('▄'));
    }
}
