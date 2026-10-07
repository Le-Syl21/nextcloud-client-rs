/*
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testnextcloudpropagator.cpp (nextcloud/desktop v34.0.5).

use nc_dav::xml::parse_etag;
use nc_dav::{Body, Reply};
use nc_sync::propagator::{create_download_tmp_file_name, get_exception_from_reply};
use nc_testutils::server::error_reply;

/// `QString::length()`: UTF-16 code units.
fn qlen(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `tmpFileName.mid(tmpFileName.lastIndexOf('/') + 1)` when it contains a `/`.
fn file_name_part(tmp_file_name: &str) -> &str {
    match tmp_file_name.rfind('/') {
        Some(i) => &tmp_file_name[i + 1..],
        None => tmp_file_name,
    }
}

#[test]
// Upstream's `QVERIFY( true )`, kept as is.
#[allow(clippy::assertions_on_constants)]
fn test_update_error_from_session() {
    //OwncloudPropagator propagator(nullptr, QLatin1String("test1"), QLatin1String("test2"), new ProgressDatabase);
    assert!(true);
}

#[test]
fn test_tmp_download_file_name_generation() {
    let mut fn_ = String::new();
    // without dir
    for _ in 1..=1000 {
        fn_.push('F');
        let tmp_file_name = create_download_tmp_file_name(&fn_);
        let tmp_file_name = file_name_part(&tmp_file_name);
        assert!(qlen(tmp_file_name) > 0);
        assert!(qlen(tmp_file_name) <= 254);
    }
    // with absolute dir
    fn_ = "/Users/guruz/ownCloud/rocks/GPL".to_owned();
    for _ in 1..1000 {
        fn_.push('F');
        let tmp_file_name = create_download_tmp_file_name(&fn_);
        let tmp_file_name = file_name_part(&tmp_file_name);
        assert!(qlen(tmp_file_name) > 0);
        assert!(qlen(tmp_file_name) <= 254);
    }
    // with relative dir
    fn_ = "rocks/GPL".to_owned();
    for _ in 1..1000 {
        fn_.push('F');
        let tmp_file_name = create_download_tmp_file_name(&fn_);
        let tmp_file_name = file_name_part(&tmp_file_name);
        assert!(qlen(tmp_file_name) > 0);
        assert!(qlen(tmp_file_name) <= 254);
    }
}

#[test]
fn test_parse_etag() {
    let tests: Vec<(&str, &str)> = vec![
        ("\"abcd\"", "abcd"),
        ("\"\"", ""),
        ("\"fii\"-gzip", "fii"),
        ("W/\"foo\"", "foo"),
    ];

    for test in &tests {
        assert_eq!(parse_etag(test.0.as_bytes()), test.1.as_bytes());
    }
}

#[test]
fn test_parse_exception() {
    let url = "http://cloud.example.de/";
    let body = bytes::Bytes::from_static(
        b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n\
<s:exception>Sabre\\Exception\\UnsupportedMediaType</s:exception>\n\
<s:message>Virus detected!</s:message>\n\
</d:error>",
    );
    // `new FakeErrorReply(PutOperation, request, this, 415, body)`, as the
    // `Reply` the jobs build from it.
    let (parts, reply_body) = error_reply(415, body.clone()).into_parts();
    let mut reply = Reply::from_parts(url.to_owned(), String::new(), &parts);
    match reply_body {
        Body::Full(b) => reply.body = b,
        Body::Stream { .. } => unreachable!("FakeErrorReply has an in-memory body"),
    }
    let exception_parsed = get_exception_from_reply(&reply);
    // verify parsing succeeded
    assert!(!exception_parsed.0.is_empty());
    assert!(!exception_parsed.1.is_empty());
    // verify buffer is not changed
    assert_eq!(reply.body.len(), body.len());
}

// The two following tests are compiled upstream only `#ifdef
// HAVE_QHTTPSERVER` and run `GETFileJob` against real local HTTP servers.

#[test]
#[ignore = "pending: GETFileJob decompression safety threshold (setDecompressionThresholdBase) not ported"]
fn test_get_file_job_decompression_threshold() {
    // Upstream serves 50 MiB of 'A' as a raw deflate stream and expects:
    // - with the default threshold (20 MiB): no finishedSignal, reply error
    //   UnknownContentError, and fewer bytes received than
    //   decompressedSafetyCheckThreshold();
    // - with setDecompressionThresholdBase(50 MiB): finishedSignal, NoError
    //   and the whole 50 MiB received.
    // The port's GET (reqwest with gzip/deflate decoding) has no
    // decompression ratio check and no threshold setting.
    unimplemented!("decompression safety threshold not ported");
}

#[test]
#[ignore = "pending: direct download URLs (GETFileJob with an explicit QUrl) not ported"]
fn test_direct_url_credentials() {
    // Upstream checks that GETFileJob sends no Authorization header to a
    // direct download server (also after a redirect), and does send it on
    // the standard DAV path, while a DAV redirect to another origin drops
    // it. The port's download job only targets the account's DAV URL
    // (`SyncFileItem::direct_download_url` is never used).
    unimplemented!("direct download URLs not ported");
}
