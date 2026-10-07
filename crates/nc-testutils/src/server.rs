/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-FileCopyrightText: 2020 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! The remote side: `FakeQNAM` and the `Fake*Reply` classes, as an
//! in-process [`Transport`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderValue, StatusCode};
use nc_dav::{
    Body, BoxFuture, NetworkError, ReplyOverride, Request, Response, Transport, TransportError,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::file_info::{EtagsAction, FileInfo, LockState};
use crate::path::PathComponents;
use crate::util::{from_secs, http_date, rand};

/// `sRootUrl`.
pub const ROOT_URL: &str = "owncloud://somehost/owncloud/remote.php/dav/";
/// `sRootUrl2`: the files root of user `admin`.
pub const ROOT_URL2: &str = "owncloud://somehost/owncloud/remote.php/dav/files/admin/";
/// `sUploadUrl`: the chunked upload root of user `admin`.
pub const UPLOAD_URL: &str = "owncloud://somehost/owncloud/remote.php/dav/uploads/admin/";

const ROOT_PATH: &str = "/owncloud/remote.php/dav/";
const ROOT2_PATH: &str = "/owncloud/remote.php/dav/files/admin/";
const UPLOAD_PATH: &str = "/owncloud/remote.php/dav/uploads/admin/";

const DAV_URI: &str = "DAV:";
const OC_URI: &str = "http://owncloud.org/ns";
const NC_URI: &str = "http://nextcloud.org/ns";

/// Characters `QUrl::toPercentEncoding(s, "/")` leaves alone: unreserved
/// (`ALPHA DIGIT - . _ ~`) plus `/`.
const PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// The decoded path of a URI (`QUrl::path()`).
fn decoded_path(uri: &http::Uri) -> String {
    percent_decode_str(uri.path())
        .decode_utf8_lossy()
        .into_owned()
}

/// `getFilePathFromUrl`: the path relative to the files root, the uploads
/// root or the DAV root (checked in that order). `None` (upstream: a null
/// string) when the URI is outside all three.
pub fn file_path_from_url(uri: &http::Uri) -> Option<String> {
    let path = decoded_path(uri);
    [ROOT2_PATH, UPLOAD_PATH, ROOT_PATH]
        .into_iter()
        .find_map(|prefix| path.strip_prefix(prefix).map(str::to_owned))
}

/// What an [`Override`] (or a handler) answers.
#[derive(Debug)]
pub enum FakeReply {
    /// A complete HTTP response (any status).
    Response(Response),
    /// A failure below HTTP (e.g. `OperationCanceledError`).
    Error(TransportError),
    /// Never completes (`FakeHangingReply`); the client must abort or time out.
    Hang,
    /// A response delivered after a delay (`DelayedReply<T>`).
    Delayed(std::time::Duration, Response),
}

impl From<Response> for FakeReply {
    fn from(r: Response) -> Self {
        Self::Response(r)
    }
}

/// `FakeQNAM::Override`: inspects every request first and may answer it.
/// Returning `None` falls through to the error paths and the default
/// handlers. The override gets the whole server state, so it can read or
/// mutate the remote tree like upstream lambdas capturing the `FakeFolder`.
pub type Override = Box<dyn FnMut(&Request, &mut ServerState) -> Option<FakeReply> + Send>;

/// Mutable state of the fake server (the members of `FakeQNAM`).
pub struct ServerState {
    pub remote_root: FileInfo,
    /// Tree of the chunked uploads (`/dav/uploads/admin/`).
    pub upload_root: FileInfo,
    /// Maps a path to an HTTP error status (`errorPaths`).
    pub error_paths: HashMap<String, u16>,
    pub server_version: String,
    override_: Option<Override>,
}

impl ServerState {
    pub fn set_override(&mut self, o: Option<Override>) {
        self.override_ = o;
    }
}

/// `FakeQNAM`: an in-memory WebDAV server implementing [`Transport`].
/// Cheap to clone (shared state).
#[derive(Clone)]
pub struct FakeServer {
    state: Arc<Mutex<ServerState>>,
}

impl FakeServer {
    /// `FakeQNAM(initialRoot)`; server version `10.0.0` like upstream.
    pub fn new(initial_root: FileInfo) -> Self {
        Self {
            state: Arc::new(Mutex::new(ServerState {
                remote_root: initial_root,
                upload_root: FileInfo::default(),
                error_paths: HashMap::new(),
                server_version: "10.0.0".to_owned(),
                override_: None,
            })),
        }
    }

    /// Locks the server state (poisoning from a failed test is ignored).
    pub fn state(&self) -> MutexGuard<'_, ServerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `setOverride`.
    pub fn set_override(
        &self,
        o: impl FnMut(&Request, &mut ServerState) -> Option<FakeReply> + Send + 'static,
    ) {
        self.state().override_ = Some(Box::new(o));
    }

    pub fn clear_override(&self) {
        self.state().override_ = None;
    }

    /// `setServerVersion`.
    pub fn set_server_version(&self, version: &str) {
        self.state().server_version = version.to_owned();
    }

    /// Handles one request synchronously (`FakeQNAM::createRequest`).
    pub fn handle(&self, mut request: Request) -> FakeReply {
        // Upstream's FakeQNAM replaces `AccessManager`, so it sets the
        // `X-Request-ID` header itself. Here the requests come through
        // `Account::build_request` (the `AccessManager` equivalent), which
        // already set one: keep it, so that the request id the job reports
        // (`AbstractNetworkJob::requestId()`) is the one the server saw.
        if !request.headers().contains_key("X-Request-ID") {
            let request_id = format!("{:08x}{:08x}", rand(), rand());
            request
                .headers_mut()
                .insert("X-Request-ID", HeaderValue::from_str(&request_id).unwrap());
        }
        let mut guard = self.state();
        let state = &mut *guard;

        if let Some(mut o) = state.override_.take() {
            let reply = o(&request, state);
            if state.override_.is_none() {
                state.override_ = Some(o);
            }
            if let Some(reply) = reply {
                return reply;
            }
        }

        let file_name = file_path_from_url(request.uri());
        if let Some(&code) = file_name
            .as_ref()
            .and_then(|name| state.error_paths.get(name))
        {
            return error_reply(code, Bytes::new()).into();
        }
        // Upstream asserts that every request targets the DAV tree; requests
        // elsewhere (OCS...) must be answered by an override.
        let Some(file_name) = file_name else {
            return status_reply(StatusCode::NOT_FOUND).into();
        };

        let is_upload = decoded_path(request.uri()).starts_with(UPLOAD_PATH);
        let method = request.method().as_str().to_owned();
        let ServerState {
            remote_root,
            upload_root,
            ..
        } = state;
        let info = if is_upload {
            &mut *upload_root
        } else {
            &mut *remote_root
        };

        match method.as_str() {
            "PROPFIND" => propfind_reply(info, &request, &file_name).into(),
            "GET" => get_reply(info, &file_name).into(),
            "PUT" => {
                if request
                    .headers()
                    .get("X-OC-Mtime")
                    .is_some_and(|m| to_long_long(m.as_bytes()) <= 0)
                {
                    return error_reply(500, Bytes::new()).into();
                }
                put_reply(info, &request, &file_name).into()
            }
            "MKCOL" => mkcol_reply(info, &file_name).into(),
            "DELETE" => delete_reply(info, &file_name).into(),
            "MOVE" if !is_upload => move_reply(info, &request, &file_name).into(),
            "MOVE" => chunk_move_reply(upload_root, remote_root, &request, &file_name).into(),
            // Bulk upload (FakePutMultiFileReply) is pending (Phase 3).
            "POST" => status_reply(StatusCode::NOT_IMPLEMENTED).into(),
            "LOCK" | "UNLOCK" => file_lock_reply(info, &request, &file_name).into(),
            // Upstream: Q_UNREACHABLE().
            _ => status_reply(StatusCode::METHOD_NOT_ALLOWED).into(),
        }
    }
}

impl Transport for FakeServer {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        // Streamed request bodies (uploads) are collected first: the fake
        // replies, like upstream's, look at the whole payload.
        let request = match request.body() {
            Body::Full(_) => Ok(request),
            Body::Stream { .. } => Err(request),
        };
        match request {
            Ok(request) => Self::reply_future(self.handle(request)),
            Err(request) => {
                let server = self.clone();
                Box::pin(async move {
                    let (parts, body) = request.into_parts();
                    let bytes = body
                        .collect()
                        .await
                        .map_err(|e| TransportError::Other(e.to_string()))?;
                    let request = http::Request::from_parts(parts, Body::Full(bytes));
                    Self::reply_future(server.handle(request)).await
                })
            }
        }
    }
}

impl FakeServer {
    fn reply_future(reply: FakeReply) -> BoxFuture<'static, Result<Response, TransportError>> {
        match reply {
            FakeReply::Response(r) => Box::pin(std::future::ready(Ok(r))),
            FakeReply::Error(e) => Box::pin(std::future::ready(Err(e))),
            FakeReply::Hang => Box::pin(std::future::pending()),
            FakeReply::Delayed(delay, r) => Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(r)
            }),
        }
    }
}

/// `QByteArray::toLongLong()`: 0 when not a valid integer.
fn to_long_long(bytes: &[u8]) -> i64 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// `FakeReply` base: every reply carries a `Date` header.
fn response(status: StatusCode) -> http::response::Builder {
    http::Response::builder()
        .status(status)
        .header("Date", http_date(SystemTime::now()))
}

pub fn status_reply(status: StatusCode) -> Response {
    response(status).body(Body::empty()).unwrap()
}

/// `FakeErrorReply`: the given status with an optional body.
pub fn error_reply(code: u16, body: Bytes) -> Response {
    let status = StatusCode::from_u16(code).expect("valid HTTP status");
    // Upstream: `setError(InternalServerError, ...)` whatever the status.
    with_override(
        response(status).body(Body::from(body)).unwrap(),
        NetworkError::InternalServerError,
        Some(code),
    )
}

/// Attaches the `QNetworkReply` error upstream's fake reply reports.
pub fn with_override(
    mut resp: Response,
    error: NetworkError,
    http_status: Option<u16>,
) -> Response {
    resp.extensions_mut()
        .insert(ReplyOverride { error, http_status });
    resp
}

/// `FakeJsonErrorReply` / `FakeJsonReply`: a JSON body with a status.
pub fn json_reply(code: u16, json: &str) -> Response {
    let status = StatusCode::from_u16(code).expect("valid HTTP status");
    response(status)
        .body(Body::from(Bytes::copy_from_slice(json.as_bytes())))
        .unwrap()
}

/// `FakePayloadReply`: 200 with a body and its Content-Length (no delay).
pub fn payload_reply(body: Bytes) -> Response {
    response(StatusCode::OK)
        .header("Content-Length", body.len())
        .body(Body::from(body))
        .unwrap()
}

fn xml_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn text_element(out: &mut String, prefix: &str, name: &str, text: &str) {
    let _ = write!(out, "<{prefix}:{name}>");
    xml_escape(text, out);
    let _ = write!(out, "</{prefix}:{name}>");
}

/// Builds the multistatus body of `FakePropfindReply` for `file_info` and its
/// direct children. The `Depth` header is ignored, as upstream does.
///
/// Deviation: hrefs are percent-encoded like a real server's, while
/// upstream's fake returns `QUrl::path()` (mostly decoded); the client decodes
/// hrefs either way.
pub fn propfind_body(file_info: &FileInfo, prefix: &str) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    let _ = write!(
        xml,
        "<d:multistatus xmlns:d=\"{DAV_URI}\" xmlns:oc=\"{OC_URI}\" xmlns:nc=\"{NC_URI}\">"
    );
    let encoded_prefix = utf8_percent_encode(prefix, PATH_ENCODE_SET).to_string();
    let write_file_response = |xml: &mut String, fi: &FileInfo| {
        xml.push_str("<d:response>");
        let mut url = utf8_percent_encode(&fi.absolute_path(), PATH_ENCODE_SET).to_string();
        if !url.ends_with('/') {
            url.push('/');
        }
        // Utility::concatUrlPath: avoid '//' and a missing '/'.
        let href = match (encoded_prefix.ends_with('/'), url.starts_with('/')) {
            (true, true) => format!("{}{}", encoded_prefix, &url[1..]),
            (false, false) => format!("{encoded_prefix}/{url}"),
            _ => format!("{encoded_prefix}{url}"),
        };
        text_element(xml, "d", "href", &href);
        xml.push_str("<d:propstat><d:prop>");
        if fi.is_dir {
            xml.push_str("<d:resourcetype><d:collection/></d:resourcetype>");
            // Upstream sums into an int.
            let total_size: i64 = fi.children.values().map(|c| c.size).sum();
            text_element(xml, "oc", "size", &(total_size as i32).to_string());
        } else {
            xml.push_str("<d:resourcetype/>");
        }
        text_element(xml, "d", "getlastmodified", &http_date(fi.last_modified));
        text_element(xml, "d", "getcontentlength", &fi.size.to_string());
        text_element(xml, "d", "getetag", &format!("\"{}\"", fi.etag));
        text_element(
            xml,
            "oc",
            "quota-available-bytes",
            &fi.folder_quota.bytes_available_string(),
        );
        text_element(
            xml,
            "oc",
            "quota-used-bytes",
            &fi.folder_quota.bytes_used.to_string(),
        );
        let permissions = if !fi.permissions.is_null() {
            fi.permissions.to_string()
        } else if fi.is_shared {
            "GSRDNVCKW".to_owned()
        } else {
            "GRDNVCKW".to_owned()
        };
        text_element(xml, "oc", "permissions", &permissions);
        if fi.is_shared {
            let value = if fi.download_forbidden {
                "false"
            } else {
                "true"
            };
            text_element(
                xml,
                "oc",
                "share-attributes",
                &format!("[{{\"scope\":\"permissions\",\"key\":\"download\",\"value\":{value}}}]"),
            );
        }
        // SharePermissionRead | Update | Create | Delete | Share
        text_element(xml, "oc", "share-permissions", "31");
        text_element(xml, "oc", "id", &fi.file_id);
        text_element(xml, "oc", "fileid", &fi.numeric_file_id());
        text_element(xml, "oc", "checksums", &fi.checksums);
        text_element(xml, "oc", "privatelink", &href);
        text_element(xml, "nc", "lock-owner", &fi.lock_owner_id);
        let locked = if fi.lock_state == LockState::FileLocked {
            "1"
        } else {
            "0"
        };
        text_element(xml, "nc", "lock", locked);
        text_element(xml, "nc", "lock-owner-type", &fi.lock_owner_id);
        text_element(xml, "nc", "lock-owner-displayname", &fi.lock_owner_id);
        text_element(xml, "nc", "lock-owner-editor", &fi.lock_owner_id);
        text_element(xml, "nc", "lock-time", &fi.lock_time.to_string());
        text_element(xml, "nc", "lock-timeout", &fi.lock_timeout.to_string());
        if !fi.lock_token.is_empty() {
            text_element(xml, "nc", "lock-token", &fi.lock_token);
        }
        text_element(
            xml,
            "nc",
            "is-encrypted",
            if fi.is_encrypted { "1" } else { "0" },
        );
        text_element(
            xml,
            "nc",
            "metadata-files-live-photo",
            if fi.is_live_photo { "1" } else { "0" },
        );
        xml.push_str(&fi.extra_dav_properties);
        xml.push_str("</d:prop>");
        text_element(xml, "d", "status", "HTTP/1.1 200 OK");
        xml.push_str("</d:propstat></d:response>");
    };
    write_file_response(&mut xml, file_info);
    for child in file_info.children.values() {
        write_file_response(&mut xml, child);
    }
    xml.push_str("</d:multistatus>\n");
    xml
}

fn request_prefix(request: &Request, file_name: &str) -> String {
    let path = decoded_path(request.uri());
    path[..path.len() - file_name.len()].to_owned()
}

/// `FakePropfindReply`: 207 with the multistatus body, or 404.
fn propfind_reply(root: &mut FileInfo, request: &Request, file_name: &str) -> Response {
    let Some(fi) = root.find(file_name) else {
        // `respond404`: status 404 with `InternalServerError`.
        return error_reply(404, Bytes::new());
    };
    let body = propfind_body(fi, &request_prefix(request, file_name));
    raw_propfind_reply(Bytes::from(body))
}

/// `FakePropfindReply(replyContents, ...)`: 207 with a custom body.
pub fn raw_propfind_reply(body: Bytes) -> Response {
    response(StatusCode::MULTI_STATUS)
        .header("Content-Length", body.len())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

fn with_file_headers(builder: http::response::Builder, fi: &FileInfo) -> http::response::Builder {
    builder
        .header("OC-ETag", fi.etag.as_str())
        .header("ETag", fi.etag.as_str())
        .header("OC-FileId", fi.file_id.as_str())
}

/// `FakeGetReply`: the file content (`size` times `contentChar`).
///
/// Deviation: upstream asserts that the file exists; here a missing file is
/// a 404 (`ContentNotFoundError`), what upstream does in release builds.
pub fn get_reply(root: &FileInfo, file_name: &str) -> Response {
    assert!(!file_name.is_empty(), "GET on the root");
    let Some(fi) = root.find(file_name) else {
        // Release build behaviour: `ContentNotFoundError` without HTTP status.
        return with_override(
            status_reply(StatusCode::NOT_FOUND),
            NetworkError::ContentNotFoundError,
            None,
        );
    };
    let size = usize::try_from(fi.size).unwrap_or(0);
    with_file_headers(response(StatusCode::OK), fi)
        .header("Content-Length", size)
        .body(Body::from(vec![fi.content_char; size]))
        .unwrap()
}

/// `FakeGetWithDataReply`: answers a GET on `file_name` with `data`,
/// honouring a `Range: bytes=start-end` header. For use in overrides.
pub fn get_with_data_reply(root: &FileInfo, data: &[u8], request: &Request) -> Response {
    assert!(!data.is_empty());
    let file_name = file_path_from_url(request.uri()).unwrap_or_default();
    assert!(!file_name.is_empty());
    let fi = root
        .find(file_name.as_str())
        .expect("FakeGetWithDataReply: file not found");
    let mut payload = data;
    let range = request
        .headers()
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_byte_range);
    if let Some((start, end)) = range {
        let start = start.min(payload.len());
        let len = (end + 1).saturating_sub(start);
        payload = &payload[start..(start + len).min(payload.len())];
    }
    with_file_headers(response(StatusCode::OK), fi)
        .header("Content-Length", payload.len())
        .body(Body::from(Bytes::copy_from_slice(payload)))
        .unwrap()
}

/// Matches `bytes=(\d+)-(\d+)` anywhere in the header value.
fn parse_byte_range(range: &str) -> Option<(usize, usize)> {
    let rest = &range[range.find("bytes=")? + "bytes=".len()..];
    let (start, rest) = rest.split_once('-')?;
    let end: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((start.parse().ok()?, end.parse().ok()?))
}

/// `FakePutReply::perform`: creates or updates the file (content = first
/// byte of the payload), sets its mtime from `X-OC-Mtime` and invalidates the
/// etags up to the root. `None` if the parent directory does not exist.
pub fn put_perform<'a>(
    root: &'a mut FileInfo,
    request: &Request,
    file_name: &str,
) -> Option<&'a mut FileInfo> {
    assert!(!file_name.is_empty(), "PUT on the root");
    let payload: &[u8] = request.body().as_bytes().map_or(&[], |b| b.as_ref());
    let size = payload.len() as i64;
    // Upstream reads at(0) of a possibly empty payload for existing files
    // (undefined); use ' ' like for new files.
    let content_char = payload.first().copied().unwrap_or(b' ');
    if let Some(fi) = root.find_mut(file_name) {
        fi.size = size;
        fi.content_char = content_char;
    } else {
        root.create(file_name, size, content_char)?;
    }
    let mtime = request
        .headers()
        .get("X-OC-Mtime")
        .map_or(0, |v| to_long_long(v.as_bytes()));
    root.find_mut(file_name)?.last_modified = from_secs(mtime);
    root.find_with(file_name, EtagsAction::Invalidate)
}

/// `FakePutReply`: 200 with the new etag and file id, or 412 when the file
/// could not be created.
fn put_reply(root: &mut FileInfo, request: &Request, file_name: &str) -> Response {
    let Some(fi) = put_perform(root, request, file_name) else {
        // Upstream sets the 412 status but no error.
        return with_override(
            status_reply(StatusCode::PRECONDITION_FAILED),
            NetworkError::NoError,
            Some(412),
        );
    };
    with_file_headers(response(StatusCode::OK), fi)
        // Prevents the propagator from issuing a separate mtime update.
        .header("X-OC-MTime", "accepted")
        .body(Body::empty())
        .unwrap()
}

/// `FakeMkcolReply`: 201 with `OC-FileId`.
///
/// Deviation: when the parent is missing upstream calls a no-op `abort()`
/// and the reply never finishes; here it is a 409 Conflict like a real server.
fn mkcol_reply(root: &mut FileInfo, file_name: &str) -> Response {
    assert!(!file_name.is_empty(), "MKCOL on the root");
    let Some(fi) = root.create_dir(file_name) else {
        return status_reply(StatusCode::CONFLICT);
    };
    response(StatusCode::CREATED)
        .header("OC-FileId", fi.file_id.as_str())
        .body(Body::empty())
        .unwrap()
}

/// `FakeDeleteReply`: 204. (A missing parent, an assertion upstream, is a 404.)
fn delete_reply(root: &mut FileInfo, file_name: &str) -> Response {
    assert!(!file_name.is_empty(), "DELETE on the root");
    let components = PathComponents::new(file_name);
    if root.find(components.parent_dir_components()).is_none() {
        return status_reply(StatusCode::NOT_FOUND);
    }
    crate::FileModifier::remove(root, file_name);
    status_reply(StatusCode::NO_CONTENT)
}

fn destination_path(request: &Request) -> Option<String> {
    let dest = request.headers().get("Destination")?.to_str().ok()?;
    file_path_from_url(&dest.parse().ok()?)
}

/// `FakeMoveReply`: renames within the files tree, 201.
fn move_reply(root: &mut FileInfo, request: &Request, file_name: &str) -> Response {
    assert!(!file_name.is_empty(), "MOVE of the root");
    let dest = destination_path(request).expect("MOVE without a valid Destination");
    assert!(!dest.is_empty());
    crate::FileModifier::rename(root, file_name, &dest);
    status_reply(StatusCode::CREATED)
}

/// `FakeChunkMoveReply::perform`: assembles the chunks of
/// `uploads/<transfer>/` (a MOVE of `<transfer>/.file`) into the destination
/// file. The data is not really assembled: size is the sum of the chunk sizes
/// and the content is the chunks' common content character. Returns `None`
/// when the `If` precondition does not match the destination etag.
pub fn chunk_move_perform<'a>(
    uploads: &mut FileInfo,
    remote_root: &'a mut FileInfo,
    request: &Request,
    source: &str,
) -> Option<&'a mut FileInfo> {
    let source = source
        .strip_suffix("/.file")
        .expect("chunk MOVE source must end with /.file");
    let source_folder = uploads.find(source).expect("chunk MOVE: no such upload");
    assert!(source_folder.is_dir);
    let mut size = 0i64;
    let mut payload = 0u8;
    for chunk in source_folder.children.values() {
        assert!(!chunk.is_dir);
        assert!(chunk.size > 0, "There should not be empty chunks");
        size += chunk.size;
        assert!(payload == 0 || payload == chunk.content_char);
        payload = chunk.content_char;
    }

    let file_name = destination_path(request).expect("chunk MOVE without Destination");
    assert!(!file_name.is_empty());
    let destination = request
        .headers()
        .get("Destination")
        .unwrap()
        .as_bytes()
        .to_vec();
    let if_header = request.headers().get("If").map(|v| v.as_bytes().to_vec());
    if let Some(fi) = remote_root.find_mut(file_name.as_str()) {
        // The client should put this header, conditioned on the destination.
        let if_header = if_header.expect("chunk MOVE over an existing file without If");
        let mut start = b"<".to_vec();
        start.extend_from_slice(&destination);
        start.push(b'>');
        assert!(if_header.starts_with(&start));
        let mut expected = start;
        expected.extend_from_slice(format!(" ([\"{}\"])", fi.etag).as_bytes());
        if if_header != expected {
            return None;
        }
        fi.size = size;
        fi.content_char = payload;
    } else {
        assert!(if_header.is_none());
        remote_root.create(&file_name, size, payload)?;
    }
    let mtime = request
        .headers()
        .get("X-OC-Mtime")
        .map_or(0, |v| to_long_long(v.as_bytes()));
    remote_root.find_mut(file_name.as_str())?.last_modified = from_secs(mtime);
    remote_root.find_with(file_name.as_str(), EtagsAction::Invalidate)
}

fn chunk_move_reply(
    uploads: &mut FileInfo,
    remote_root: &mut FileInfo,
    request: &Request,
    source: &str,
) -> Response {
    match chunk_move_perform(uploads, remote_root, request, source) {
        Some(fi) => with_file_headers(response(StatusCode::CREATED), fi)
            .body(Body::empty())
            .unwrap(),
        // `respondPreconditionFailed`: 412 with `InternalServerError`.
        None => error_reply(412, Bytes::new()),
    }
}

/// `FakeFileLockReply`: 207 with a `<d:prop>` describing the lock taken
/// (LOCK) or released (UNLOCK) by `admin`. Like upstream, the remote tree's
/// lock fields are not modified.
fn file_lock_reply(root: &mut FileInfo, request: &Request, file_name: &str) -> Response {
    if root.find(file_name).is_none() {
        return status_reply(StatusCode::NOT_FOUND);
    }
    let locked = if request.method().as_str() == "LOCK" {
        "1"
    } else {
        "0"
    };
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    let _ = write!(
        xml,
        "<d:prop xmlns:d=\"{DAV_URI}\" xmlns:oc=\"{OC_URI}\" xmlns:nc=\"{NC_URI}\">"
    );
    text_element(&mut xml, "nc", "lock", locked);
    text_element(&mut xml, "nc", "lock-owner-type", "0");
    text_element(&mut xml, "nc", "lock-owner", "admin");
    text_element(&mut xml, "nc", "lock-owner-displayname", "John Doe");
    text_element(&mut xml, "nc", "lock-owner-editor", "");
    text_element(&mut xml, "nc", "lock-time", "1234560");
    text_element(&mut xml, "nc", "lock-timeout", "1800");
    xml.push_str("</d:prop>\n");
    raw_propfind_reply(Bytes::from(xml))
}

#[cfg(test)]
mod tests;
