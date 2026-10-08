// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the network jobs of upstream `src/libsync/networkjobs.cpp`,
// `abstractnetworkjob.cpp`, `deletejob.cpp`, `propagateremotemove.cpp`
// (`MoveJob`), `propagateupload.cpp` (`PUTFileJob`) and
// `propagatedownload.cpp` (`GETFileJob`, the HTTP part) of nextcloud/desktop
// v34.0.5.

//! Network jobs as async functions.
//!
//! Each upstream job class becomes a function that builds the request
//! exactly like the job's `start()` and turns the finished reply into a
//! [`Reply`] (plus the parsed payload), like the job's `finished()`.
//! Timeouts (`AbstractNetworkJob::httpTimeout`, `OWNCLOUD_TIMEOUT`, 300 s by
//! default) and aborts (`QNetworkReply::abort()`, through a
//! [`CancellationToken`]) end the request with `OperationCanceledError`,
//! as upstream's timer does.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use tokio_util::sync::CancellationToken;

use crate::account::{Account, to_percent_encoding_keep_slash};
use crate::reply::{NetworkError, Reply};
use crate::transport::{Body, BodyStream, TransportError, method};
use crate::xml::{self, LsColListing, PropertyMap};

thread_local! {
    static HTTP_TIMEOUT_OVERRIDE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Assigns `AbstractNetworkJob::httpTimeout` (seconds); `0` goes back to the
/// `OWNCLOUD_TIMEOUT` default. Returns the previous assignment. Upstream's
/// variable is a process-wide static that tests change; here it is per
/// thread, so that tests running in parallel threads (each sync runs on its
/// test's thread) do not see each other's value.
pub fn set_http_timeout(secs: u64) -> u64 {
    HTTP_TIMEOUT_OVERRIDE.with(|t| t.replace(secs))
}

/// `AbstractNetworkJob::httpTimeout`: `OWNCLOUD_TIMEOUT` seconds, 300 by default.
pub fn http_timeout() -> Duration {
    let assigned = HTTP_TIMEOUT_OVERRIDE.with(std::cell::Cell::get);
    if assigned > 0 {
        return Duration::from_secs(assigned);
    }
    let secs = std::env::var("OWNCLOUD_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(300);
    Duration::from_secs(secs)
}

/// Per-request options.
#[derive(Clone, Debug)]
pub struct JobOptions {
    /// Aborts the request (`QNetworkReply::abort()`).
    pub cancel: Option<CancellationToken>,
    /// The job timeout (reset by network activity for streamed bodies).
    pub timeout: Duration,
    /// `AbstractCredentials::DontAddCredentialsAttribute`: no
    /// `Authorization` header (direct download URLs).
    pub dont_add_credentials: bool,
    /// `QNetworkRequest::setDecompressedSafetyCheckThreshold` (`None`: Qt's
    /// default of 10 MiB).
    pub decompressed_safety_check_threshold: Option<i64>,
    /// `setIgnoreCredentialFailure(true)`: a 401 is left to the caller and
    /// does not call `Account::handleInvalidCredentials()`.
    pub ignore_credential_failure: bool,
    /// `setFollowRedirects(false)` when false: a 3xx reply is returned as is.
    pub follow_redirects: bool,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            cancel: None,
            timeout: http_timeout(),
            dont_add_credentials: false,
            decompressed_safety_check_threshold: None,
            ignore_credential_failure: false,
            follow_redirects: true,
        }
    }
}

impl JobOptions {
    pub fn with_cancel(cancel: CancellationToken) -> Self {
        Self {
            cancel: Some(cancel),
            ..Self::default()
        }
    }
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A path relative to the DAV root of the user (`makeDavUrl(path)`).
    Dav(String),
    /// A path relative to the account URL (`makeAccountUrl(path)`).
    Account(String),
    /// A decoded absolute path on the server (an explicit `QUrl`).
    Absolute(String),
    /// A full URL, possibly on another host (a direct download URL,
    /// `QUrl::fromUserInput(url)`).
    Url(String),
}

impl Target {
    /// The decoded absolute path (the URL itself for [`Target::Url`]).
    pub fn path(&self, account: &Account) -> String {
        match self {
            Self::Dav(p) => account.dav_path_for(p),
            Self::Account(p) => account.account_path_for(p),
            Self::Absolute(p) | Self::Url(p) => p.clone(),
        }
    }

    /// The request URI.
    pub fn uri(&self, account: &Account) -> Result<http::Uri, String> {
        match self {
            Self::Url(u) => crate::account::url_from_user_input(u).map_err(|e| e.to_string()),
            _ => account.uri(&self.path(account)).map_err(|e| e.to_string()),
        }
    }
}

/// Builds the request of a job: the account's common headers, the
/// credentials unless `dont_add_credentials`, and the request attributes.
fn build_job_request(
    account: &Account,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Body,
    opts: &JobOptions,
) -> (crate::transport::Request, String) {
    let (mut req, request_id) =
        account.build_request_with(method, uri, headers, body, !opts.dont_add_credentials);
    if let Some(t) = opts.decompressed_safety_check_threshold {
        req.extensions_mut()
            .insert(crate::transport::DecompressedSafetyCheckThreshold(t));
    }
    mark_request(&mut req, opts);
    (req, request_id)
}

/// The request attributes of `opts` that every job sets the same way.
fn mark_request(req: &mut crate::transport::Request, opts: &JobOptions) {
    if opts.ignore_credential_failure {
        req.extensions_mut()
            .insert(crate::transport::IgnoreCredentialFailure);
    }
    if !opts.follow_redirects {
        req.extensions_mut()
            .insert(crate::transport::NoFollowRedirects);
    }
}

/// `HttpError` (`HttpResult` failure): HTTP code (0 if none) and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpError {
    pub code: u16,
    pub message: String,
}

fn header_map(headers: &[(&str, Vec<u8>)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// Runs `fut` with the job timeout and abort token. A timeout reports
/// [`TransportError::Timeout`], an abort [`TransportError::Canceled`].
async fn guarded<T>(
    opts: &JobOptions,
    fut: impl std::future::Future<Output = Result<T, TransportError>>,
) -> Result<T, TransportError> {
    let timed = tokio::time::timeout(opts.timeout, fut);
    let res = match &opts.cancel {
        Some(token) => {
            tokio::select! {
                biased;
                _ = token.cancelled() => return Err(TransportError::Canceled),
                r = timed => r,
            }
        }
        None => timed.await,
    };
    res.unwrap_or(Err(TransportError::Timeout))
}

/// Turns a transport failure into the reply upstream sees: a job timeout or
/// an abort is `OperationCanceledError` (the timer aborts the reply).
fn transport_failure(url: String, request_id: String, e: &TransportError) -> Reply {
    let mut reply = Reply::from_transport_error(url, request_id, e);
    if matches!(e, TransportError::Timeout) {
        reply.error = NetworkError::OperationCanceledError;
        reply.timed_out = true;
    }
    reply
}

/// Sends a request and reads the whole response body.
pub async fn send(
    account: &Account,
    method: Method,
    target: &Target,
    headers: HeaderMap,
    body: Body,
    opts: &JobOptions,
) -> Reply {
    let uri = match target.uri(account) {
        Ok(u) => u,
        Err(e) => {
            return transport_failure(
                target.path(account),
                String::new(),
                &TransportError::Other(e),
            );
        }
    };
    let url = uri.to_string();
    let mut resend = Http2Resend::new(&body);
    let mut body = Some(body);
    loop {
        let (req, request_id) = build_job_request(
            account,
            method.clone(),
            uri.clone(),
            headers.clone(),
            body.take().unwrap_or_default(),
            opts,
        );
        let result = guarded(opts, async {
            let resp = account.send(req).await?;
            let (parts, body) = resp.into_parts();
            let bytes = body
                .collect()
                .await
                .map_err(|e| crate::transport::body_stream_error(&e))?;
            Ok((parts, bytes))
        })
        .await;
        let reply = match result {
            Ok((parts, bytes)) => {
                let mut reply = Reply::from_parts(url.clone(), request_id, &parts);
                reply.body = bytes;
                reply
            }
            Err(e) => transport_failure(url.clone(), request_id, &e),
        };
        if resend.should_resend(&reply) {
            continue;
        }
        return reply;
    }
}

/// `AbstractNetworkJob::slotFinished`: Qt doesn't yet transparently resend
/// HTTP2 requests, do so here.
struct Http2Resend {
    /// Upstream refuses to resend a request with a (non-sequential) body:
    /// only body-less requests are resent.
    possible: bool,
    count: u32,
}

impl Http2Resend {
    const MAX_HTTP2_RESENDS: u32 = 3;

    fn new(body: &Body) -> Self {
        Self {
            possible: body.as_bytes().is_some_and(|b| b.is_empty()),
            count: 0,
        }
    }

    fn should_resend(&mut self, reply: &Reply) -> bool {
        if reply.error != NetworkError::ContentReSendError || !reply.http2_was_used {
            return false;
        }
        if !self.possible {
            log::warn!(target: "nextcloud.sync.networkjob", "Can't resend HTTP2 request, verb or body not suitable {}", reply.url);
            false
        } else if self.count >= Self::MAX_HTTP2_RESENDS {
            log::warn!(target: "nextcloud.sync.networkjob", "Not resending HTTP2 request, number of resends exhausted {} {}", reply.url, self.count);
            false
        } else {
            log::info!(target: "nextcloud.sync.networkjob", "HTTP2 resending {}", reply.url);
            self.count += 1;
            true
        }
    }
}

/// A response whose body is still to be read (`GETFileJob`).
pub struct StreamingReply {
    pub reply: Reply,
    pub body: BodyStream,
}

/// Sends a request and returns the response head and the body stream.
/// Errors (including HTTP >= 400) have their body read into the reply.
pub async fn send_streaming(
    account: &Account,
    method: Method,
    target: &Target,
    headers: HeaderMap,
    body: Body,
    opts: &JobOptions,
) -> Result<StreamingReply, Reply> {
    let uri = match target.uri(account) {
        Ok(u) => u,
        Err(e) => {
            return Err(transport_failure(
                target.path(account),
                String::new(),
                &TransportError::Other(e),
            ));
        }
    };
    let url = uri.to_string();
    let mut resend = Http2Resend::new(&body);
    let mut body = Some(body);
    loop {
        let (req, request_id) = build_job_request(
            account,
            method.clone(),
            uri.clone(),
            headers.clone(),
            body.take().unwrap_or_default(),
            opts,
        );
        match guarded(opts, account.send(req)).await {
            Ok(resp) => {
                let (parts, body) = resp.into_parts();
                let reply = Reply::from_parts(url.clone(), request_id, &parts);
                if resend.should_resend(&reply) {
                    continue;
                }
                return Ok(StreamingReply {
                    reply,
                    body: body.into_stream(),
                });
            }
            Err(e) => return Err(transport_failure(url.clone(), request_id, &e)),
        }
    }
}

/// Reads the next chunk of a streamed body with the job timeout and abort.
/// `Ok(None)` at the end.
pub async fn next_chunk(
    body: &mut BodyStream,
    opts: &JobOptions,
) -> Result<Option<Bytes>, TransportError> {
    guarded(opts, async {
        match body.next().await {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(crate::transport::body_stream_error(&e)),
            None => Ok(None),
        }
    })
    .await
}

/// Builds the `<d:prop>` children of a PROPFIND body like `LsColJob::start`.
fn lscol_prop_string(properties: &[&str]) -> String {
    let mut prop_str = String::new();
    for prop in properties {
        if let Some(col) = prop.rfind(':') {
            let ns = &prop[..col];
            let name = &prop[col + 1..];
            if ns == "http://owncloud.org/ns" {
                prop_str.push_str(&format!("    <oc:{name} />\n"));
            } else {
                prop_str.push_str(&format!("    <{name} xmlns=\"{ns}\" />\n"));
            }
        } else {
            prop_str.push_str(&format!("    <d:{prop} />\n"));
        }
    }
    prop_str
}

/// The default PROPFIND properties of a discovery (`LsColJob::defaultProperties`).
pub fn default_lscol_properties(root_folder: bool, account: &Account) -> Vec<&'static str> {
    let mut props = vec![
        "resourcetype",
        "getlastmodified",
        "getcontentlength",
        "getetag",
        "quota-available-bytes",
        "quota-used-bytes",
        "http://owncloud.org/ns:size",
        "http://owncloud.org/ns:id",
        "http://owncloud.org/ns:fileid",
        "http://owncloud.org/ns:downloadURL",
        "http://owncloud.org/ns:dDC",
        "http://owncloud.org/ns:permissions",
        "http://owncloud.org/ns:checksums",
        "http://nextcloud.org/ns:is-encrypted",
        "http://nextcloud.org/ns:metadata-files-live-photo",
        "http://nextcloud.org/ns:share-attributes",
    ];
    if root_folder {
        props.push("http://owncloud.org/ns:data-fingerprint");
    }
    if account.server_version_int() >= crate::account::make_server_version(10, 0, 0) {
        // Server older than 10.0 have performances issue if we ask for the
        // share-types on every PROPFIND
        props.push("http://owncloud.org/ns:share-types");
    }
    if account.capabilities().files_lock_available() {
        props.extend([
            "http://nextcloud.org/ns:lock",
            "http://nextcloud.org/ns:lock-owner-displayname",
            "http://nextcloud.org/ns:lock-owner",
            "http://nextcloud.org/ns:lock-owner-type",
            "http://nextcloud.org/ns:lock-owner-editor",
            "http://nextcloud.org/ns:lock-time",
            "http://nextcloud.org/ns:lock-timeout",
            "http://nextcloud.org/ns:lock-token",
        ]);
    }
    props.push("http://nextcloud.org/ns:is-mount-root");
    props
}

/// `LsColJob`: a Depth 1 PROPFIND. On success, the listing (in document
/// order) and the reply; on failure (`finishedWithError`) the reply.
///
/// A reply with status 207 but an invalid content type or an unparsable
/// body is a failure with `error == NoError`, like upstream.
pub async fn ls_col(
    account: &Account,
    target: &Target,
    properties: &[&str],
    opts: &JobOptions,
) -> Result<(LsColListing, Reply), Reply> {
    if properties.is_empty() {
        log::warn!(target: "nextcloud.sync.networkjob.lscol", "Propfind with no properties!");
    }
    let xml = format!(
        "<?xml version=\"1.0\" ?>\n<d:propfind xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\">\n  <d:prop>\n{}  </d:prop>\n</d:propfind>\n",
        lscol_prop_string(properties)
    );
    let headers = header_map(&[("Depth", b"1".to_vec())]);
    let expected_path = target.path(account);
    let reply = send(
        account,
        method::propfind(),
        target,
        headers,
        Body::from(xml),
        opts,
    )
    .await;
    let content_type = reply.content_type();
    let valid_content_type = content_type.contains("application/xml; charset=utf-8")
        || content_type.contains("application/xml; charset=\"utf-8\"")
        || content_type.contains("text/xml; charset=utf-8")
        || content_type.contains("text/xml; charset=\"utf-8\"");
    if reply.http_status == 207 && valid_content_type {
        match xml::parse_lscol(&reply.body, &expected_path) {
            Ok(listing) => Ok((listing, reply)),
            Err(_) => Err(reply),
        }
    } else {
        Err(reply)
    }
}

/// `RequestEtagJob`: a Depth 0 PROPFIND of `getetag`.
pub async fn request_etag(
    account: &Account,
    path: &str,
    opts: &JobOptions,
) -> Result<Vec<u8>, HttpError> {
    let xml = "<?xml version=\"1.0\" ?>\n<d:propfind xmlns:d=\"DAV:\">\n  <d:prop>\n    <d:getetag/>\n  </d:prop>\n</d:propfind>\n";
    let headers = header_map(&[("Depth", b"0".to_vec())]);
    let reply = send(
        account,
        method::propfind(),
        &Target::Dav(path.to_owned()),
        headers,
        Body::from(xml),
        opts,
    )
    .await;
    if reply.http_status == 207 {
        Ok(xml::parse_request_etag(&reply.body))
    } else {
        Err(HttpError {
            code: reply.http_status,
            message: reply.error_string(),
        })
    }
}

/// `PropfindJob`: a Depth 0 PROPFIND of `properties` (names with namespace
/// as `ns:name`, or bare DAV: names). On success the property map, on
/// failure (`finishedWithError`) the reply.
pub async fn propfind(
    account: &Account,
    path: &str,
    properties: &[&str],
    opts: &JobOptions,
) -> Result<PropertyMap, Reply> {
    let mut prop_str = String::new();
    for prop in properties {
        if let Some(col) = prop.rfind(':') {
            prop_str.push_str(&format!(
                "    <{} xmlns=\"{}\" />\n",
                &prop[col + 1..],
                &prop[..col]
            ));
        } else {
            prop_str.push_str(&format!("    <d:{prop} />\n"));
        }
    }
    let xml = format!(
        "<?xml version=\"1.0\" ?>\n<d:propfind xmlns:d=\"DAV:\">\n  <d:prop>\n{prop_str}  </d:prop>\n</d:propfind>\n"
    );
    let headers = header_map(&[("Depth", b"0".to_vec())]);
    let reply = send(
        account,
        method::propfind(),
        &Target::Dav(path.to_owned()),
        headers,
        Body::from(xml),
        opts,
    )
    .await;
    if reply.http_status == 207 {
        xml::parse_propfind(&reply.body).map_err(|_| reply)
    } else {
        Err(reply)
    }
}

/// `MkColJob`: `MKCOL` with `Content-Length: 0` and extra headers.
pub async fn mkcol(
    account: &Account,
    target: &Target,
    extra_headers: &[(&str, Vec<u8>)],
    opts: &JobOptions,
) -> Reply {
    let mut headers = header_map(&[("Content-Length", b"0".to_vec())]);
    headers.extend(header_map(extra_headers));
    send(
        account,
        method::mkcol(),
        target,
        headers,
        Body::empty(),
        opts,
    )
    .await
}

/// `DeleteJob`.
pub async fn delete(
    account: &Account,
    target: &Target,
    extra_headers: &[(&str, Vec<u8>)],
    skip_trashbin: bool,
    opts: &JobOptions,
) -> Reply {
    let mut headers = header_map(extra_headers);
    if skip_trashbin {
        headers.insert("X-NC-Skip-Trashbin", HeaderValue::from_static("true"));
    }
    send(
        account,
        method::DELETE,
        target,
        headers,
        Body::empty(),
        opts,
    )
    .await
}

/// `MoveJob`: `MOVE` with `Destination: toPercentEncoding(destination, "/")`.
pub async fn move_(
    account: &Account,
    target: &Target,
    destination: &str,
    extra_headers: &[(&str, Vec<u8>)],
    opts: &JobOptions,
) -> Reply {
    let mut headers = header_map(&[(
        "Destination",
        to_percent_encoding_keep_slash(destination).into_bytes(),
    )]);
    headers.extend(header_map(extra_headers));
    send(
        account,
        method::move_(),
        target,
        headers,
        Body::empty(),
        opts,
    )
    .await
}

/// `PUTFileJob`.
pub async fn put(
    account: &Account,
    target: &Target,
    headers: &[(&str, Vec<u8>)],
    body: Body,
    opts: &JobOptions,
) -> Reply {
    let mut map = header_map(headers);
    if let Some(len) = body.len_hint() {
        map.insert(http::header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    send(account, method::PUT, target, map, body, opts).await
}

/// `JsonApiJob`: an OCS request (`?format=json`, `OCS-APIREQUEST: true`).
/// Returns the JSON document (if it parsed) and the OCS status code.
pub async fn json_api(
    account: &Account,
    path: &str,
    opts: &JobOptions,
) -> (Option<serde_json::Value>, i32, Reply) {
    json_api_request(account, Method::GET, path, None, opts).await
}

/// [`json_api`] with another verb (`setVerb`) and a JSON body (`setBody`,
/// sent as `application/json`).
pub async fn json_api_request(
    account: &Account,
    verb: Method,
    path: &str,
    body: Option<&serde_json::Value>,
    opts: &JobOptions,
) -> (Option<serde_json::Value>, i32, Reply) {
    let mut p = path.to_owned();
    p.push_str(if p.contains('?') {
        "&format=json"
    } else {
        "?format=json"
    });
    let mut headers = header_map(&[("OCS-APIREQUEST", b"true".to_vec())]);
    // QJsonDocument::toJson(): indented.
    let body = match body.and_then(|b| serde_json::to_vec_pretty(b).ok()) {
        Some(bytes) if !bytes.is_empty() => {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            Body::from(bytes)
        }
        _ => Body::empty(),
    };
    // The query is kept out of the percent-encoded path.
    let (base, query) = p.split_once('?').unwrap_or((&p, ""));
    let path = account.account_path_for(base);
    let uri_str = format!(
        "{}?{}",
        account
            .uri(&path)
            .map(|u| u.to_string())
            .unwrap_or_default(),
        query
    );
    let reply = match uri_str.parse::<http::Uri>() {
        Ok(uri) => {
            let url = uri.to_string();
            let (mut req, request_id) = account.build_request(verb, uri, headers, body);
            mark_request(&mut req, opts);
            match guarded(opts, async {
                let resp = account.send(req).await?;
                let (parts, body) = resp.into_parts();
                let bytes = body
                    .collect()
                    .await
                    .map_err(|e| TransportError::RemoteHostClosed(e.to_string()))?;
                Ok((parts, bytes))
            })
            .await
            {
                Ok((parts, bytes)) => {
                    let mut r = Reply::from_parts(url, request_id, &parts);
                    r.body = bytes;
                    r
                }
                Err(e) => transport_failure(url, request_id, &e),
            }
        }
        Err(e) => transport_failure(
            uri_str,
            String::new(),
            &TransportError::Other(e.to_string()),
        ),
    };
    if !reply.is_ok() {
        let status = reply.http_status as i32;
        return (None, status, reply);
    }
    let text = String::from_utf8_lossy(&reply.body).into_owned();
    let mut status_code = 0;
    if text.contains("<?xml version=\"1.0\"?>") {
        if let Some(code) = capture_number(&text, "<statuscode>") {
            status_code = code;
        }
    } else if text.is_empty() && reply.http_status == 304 {
        status_code = 304;
    } else if let Some(code) = capture_number(&text, "\"statuscode\":") {
        status_code = code;
    }
    let json = serde_json::from_str(&text).ok();
    (json, status_code, reply)
}

fn capture_number(text: &str, prefix: &str) -> Option<i32> {
    let start = text.find(prefix)? + prefix.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `CheckRedirectCostFreeUrlJob`: `GET index.php/204` without following
/// redirects. Returns the HTTP status code (`jobFinished(statusCode)`); a
/// transport failure is returned as the reply (`timed_out` for the job's
/// `timeout` signal).
pub async fn check_redirect_cost_free_url(
    account: &Account,
    opts: &JobOptions,
) -> Result<u16, Reply> {
    let path = Target::Account("index.php/204".to_owned()).path(account);
    let uri = match account.uri(&path) {
        Ok(u) => u,
        Err(e) => {
            return Err(transport_failure(
                path,
                String::new(),
                &TransportError::Other(e.to_string()),
            ));
        }
    };
    let url = uri.to_string();
    let (mut req, request_id) =
        account.build_request(Method::GET, uri, HeaderMap::new(), Body::empty());
    req.extensions_mut()
        .insert(crate::transport::NoFollowRedirects);
    // CheckRedirectCostFreeUrlJob: setIgnoreCredentialFailure(true)
    req.extensions_mut()
        .insert(crate::transport::IgnoreCredentialFailure);
    let result = guarded(opts, async {
        let resp = account.send(req).await?;
        let (parts, body) = resp.into_parts();
        let _ = body.collect().await;
        Ok(parts)
    })
    .await;
    match result {
        Ok(parts) => {
            let status = parts.status.as_u16();
            if (301..=307).contains(&status) {
                log::debug!(target: "nextcloud.sync.networkjob.checkredirectcostfreeurl", "Redirecting cost-free URL {url} to {}", parts.headers.get(http::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default());
            }
            Ok(status)
        }
        Err(e) => {
            if matches!(e, TransportError::Timeout) {
                log::debug!(target: "nextcloud.sync.networkjob.checkredirectcostfreeurl", "TIMEOUT");
            }
            Err(transport_failure(url, request_id, &e))
        }
    }
}

/// `CheckServerJob`: `GET status.php`. Returns the JSON object.
pub async fn check_server(
    account: &Account,
    opts: &JobOptions,
) -> Result<serde_json::Value, Reply> {
    // CheckServerJob: setIgnoreCredentialFailure(true)
    let opts = &JobOptions {
        ignore_credential_failure: true,
        ..opts.clone()
    };
    let reply = send(
        account,
        Method::GET,
        &Target::Account("status.php".to_owned()),
        HeaderMap::new(),
        Body::empty(),
        opts,
    )
    .await;
    if !reply.is_ok() {
        return Err(reply);
    }
    match serde_json::from_slice::<serde_json::Value>(&reply.body) {
        Ok(v) if v.get("installed").is_some() => Ok(v),
        _ => Err(reply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_lscol_request_body() {
        let s = lscol_prop_string(&[
            "resourcetype",
            "http://owncloud.org/ns:size",
            "http://nextcloud.org/ns:is-encrypted",
        ]);
        assert_eq!(
            s,
            "    <d:resourcetype />\n    <oc:size />\n    <is-encrypted xmlns=\"http://nextcloud.org/ns\" />\n"
        );
    }

    #[test]
    fn derived_capture_ocs_status() {
        assert_eq!(
            capture_number(
                r#"{"ocs":{"meta":{"status":"ok","statuscode":100}}}"#,
                "\"statuscode\":"
            ),
            Some(100)
        );
        assert_eq!(capture_number("x", "\"statuscode\":"), None);
    }
}
