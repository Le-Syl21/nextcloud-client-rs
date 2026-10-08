// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/putmultifilejob.{h,cpp}` (nextcloud/desktop
// v34.0.5), with the `QHttpMultiPart` (`RelatedType`) body it sends.

//! `PutMultiFileJob`: one `POST` carrying several files as a
//! `multipart/related` body (the bulk upload endpoint
//! `/remote.php/dav/bulk`).
//!
//! The body is built by hand like `QHttpMultiPart` does (no crate writes
//! exactly this format, and the server's parser expects it):
//!
//! ```text
//! Content-Type: multipart/related; boundary="boundary_.oOo._<32 base64 chars>"
//! MIME-Version: 1.0
//!
//! --<boundary>\r\n
//! <name>: <value>\r\n        (each part header)
//! \r\n
//! <file data>\r\n
//! --<boundary>\r\n
//! ...
//! --<boundary>--\r\n
//! ```
//!
//! The part headers are written in the order of upstream's
//! `QMap<QByteArray, QByteArray>` (byte order of the names), with the
//! names as given (HTTP header names are case-insensitive; Qt versions with
//! `QHttpHeaders` may lower-case them).

use std::collections::BTreeMap;

use base64::Engine as _;
use bytes::{BufMut, Bytes, BytesMut};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use nc_dav::{Body, JobOptions, Reply, Target};

const LOG: &str = "nextcloud.sync.networkjob.put.multi";

/// `SingleUploadFileData`: the data of one file (what its `UploadDevice`
/// reads) and the headers of its part.
#[derive(Clone, Debug)]
pub(crate) struct SingleUploadFileData {
    pub data: Bytes,
    pub headers: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// `QHttpMultiPartPrivate::boundary`: `boundary_.oOo._` followed by 24
/// random bytes in base64 (32 characters).
fn new_boundary() -> String {
    let mut random = [0u8; 24];
    fastrand::fill(&mut random);
    format!(
        "boundary_.oOo._{}",
        base64::engine::general_purpose::STANDARD.encode(random)
    )
}

/// `QNetworkAccessManagerPrivate::prepareMultipart`: the request's
/// `Content-Type` (the boundary is quoted, as RFC 2046 recommends).
pub(crate) fn content_type(boundary: &str) -> String {
    format!("multipart/related; boundary=\"{boundary}\"")
}

/// `QHttpMultiPartIODevice::readData` over all the parts.
pub(crate) fn multipart_body(boundary: &str, parts: &[SingleUploadFileData]) -> Bytes {
    let mut out = BytesMut::new();
    for part in parts {
        out.put_slice(b"--");
        out.put_slice(boundary.as_bytes());
        out.put_slice(b"\r\n");
        for (name, value) in &part.headers {
            out.put_slice(name);
            out.put_slice(b": ");
            out.put_slice(value);
            out.put_slice(b"\r\n");
        }
        out.put_slice(b"\r\n");
        out.put_slice(&part.data);
        out.put_slice(b"\r\n");
    }
    out.put_slice(b"--");
    out.put_slice(boundary.as_bytes());
    out.put_slice(b"--\r\n");
    out.freeze()
}

/// `PutMultiFileJob::start` .. `finished`: sends the files and returns the
/// reply and the size of the request body (what `uploadProgress` reports).
pub(crate) async fn put_multi_file(
    account: &nc_dav::Account,
    target: &Target,
    devices: &[SingleUploadFileData],
    opts: &JobOptions,
) -> (Reply, i64) {
    let boundary = new_boundary();
    let body = multipart_body(&boundary, devices);
    let len = body.len() as i64;
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&content_type(&boundary)) {
        headers.insert(http::header::CONTENT_TYPE, v);
    }
    // We must include the header if the message conforms to RFC 2045, see section 4 of that RFC
    headers.insert(
        HeaderName::from_static("mime-version"),
        HeaderValue::from_static("1.0"),
    );
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(len));
    let reply = nc_dav::jobs::send(
        account,
        Method::POST,
        target,
        headers,
        Body::from(body),
        opts,
    )
    .await;
    if reply.error != nc_dav::NetworkError::NoError {
        log::warn!(target: LOG, " Network error: {}", reply.error_string());
    }
    log::info!(target: LOG, "POST of {} FINISHED WITH STATUS {:?} {} {}", reply.url, reply.error, reply.http_status, reply.reason_phrase);
    (reply, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_multipart_format() {
        let boundary = new_boundary();
        assert!(boundary.starts_with("boundary_.oOo._"));
        assert_eq!(boundary.len(), "boundary_.oOo._".len() + 32);
        let mut headers = BTreeMap::new();
        headers.insert(b"X-File-Path".to_vec(), b"/A/a".to_vec());
        headers.insert(b"Content-Length".to_vec(), b"3".to_vec());
        let body = multipart_body(
            "B",
            &[SingleUploadFileData {
                data: Bytes::from_static(b"abc"),
                headers,
            }],
        );
        assert_eq!(
            &body[..],
            b"--B\r\nContent-Length: 3\r\nX-File-Path: /A/a\r\n\r\nabc\r\n--B--\r\n"
        );
        assert_eq!(content_type("B"), "multipart/related; boundary=\"B\"");
    }
}
