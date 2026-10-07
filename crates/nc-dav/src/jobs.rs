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

/// `AbstractNetworkJob::httpTimeout`: `OWNCLOUD_TIMEOUT` seconds, 300 by default.
pub fn http_timeout() -> Duration {
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
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            cancel: None,
            timeout: http_timeout(),
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
}

impl Target {
    /// The decoded absolute path.
    pub fn path(&self, account: &Account) -> String {
        match self {
            Self::Dav(p) => account.dav_path_for(p),
            Self::Account(p) => account.account_path_for(p),
            Self::Absolute(p) => p.clone(),
        }
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
    let path = target.path(account);
    let uri = match account.uri(&path) {
        Ok(u) => u,
        Err(e) => {
            return transport_failure(path, String::new(), &TransportError::Other(e.to_string()));
        }
    };
    let url = uri.to_string();
    let (req, request_id) = account.build_request(method, uri, headers, body);
    let result = guarded(opts, async {
        let resp = account.send(req).await?;
        let (parts, body) = resp.into_parts();
        let bytes = body
            .collect()
            .await
            .map_err(|e| TransportError::RemoteHostClosed(e.to_string()))?;
        Ok((parts, bytes))
    })
    .await;
    match result {
        Ok((parts, bytes)) => {
            let mut reply = Reply::from_parts(url, request_id, &parts);
            reply.body = bytes;
            reply
        }
        Err(e) => transport_failure(url, request_id, &e),
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
    let path = target.path(account);
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
    let (req, request_id) = account.build_request(method, uri, headers, body);
    match guarded(opts, account.send(req)).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            let reply = Reply::from_parts(url, request_id, &parts);
            Ok(StreamingReply {
                reply,
                body: body.into_stream(),
            })
        }
        Err(e) => Err(transport_failure(url, request_id, &e)),
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
            Some(Err(e)) => Err(TransportError::RemoteHostClosed(e.to_string())),
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
    let mut p = path.to_owned();
    p.push_str(if p.contains('?') {
        "&format=json"
    } else {
        "?format=json"
    });
    let headers = header_map(&[("OCS-APIREQUEST", b"true".to_vec())]);
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
            let (req, request_id) = account.build_request(Method::GET, uri, headers, Body::empty());
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

/// `CheckServerJob`: `GET status.php`. Returns the JSON object.
pub async fn check_server(
    account: &Account,
    opts: &JobOptions,
) -> Result<serde_json::Value, Reply> {
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
