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
// HAVE_QHTTPSERVER` and run `GETFileJob` against real local HTTP servers
// (here the minimal server of `common/http_server.rs`) through the real
// reqwest transport.

#[path = "common/http_server.rs"]
mod http_server;

use std::sync::Arc;

use http_server::{HttpResponse, HttpServer, route};
use nc_dav::{Account, HttpClientOptions, HttpTransport, NetworkError, ServerUrl, Target};
use nc_sync::propagator::{CUSTOM_DECOMPRESSED_SAFETY_CHECK_THRESHOLD, GetFileJob};
use nc_testutils::run_event_loop;

/// `qCompress(data, level)`: a zlib stream (after Qt's 4-byte size prefix,
/// which the test strips with `mid(4)`).
fn zlib_compress(data: &[u8], level: i32) -> Vec<u8> {
    let mut out = vec![0u8; zlib_rs::compress_bound(data.len()) + 64];
    let mut deflate = zlib_rs::Deflate::new(level, true, 15);
    let mut input = data;
    let mut written = 0usize;
    loop {
        let before_in = deflate.total_in();
        let before_out = deflate.total_out();
        let status = deflate
            .compress(input, &mut out[written..], zlib_rs::DeflateFlush::Finish)
            .expect("deflate");
        input = &input[(deflate.total_in() - before_in) as usize..];
        written += (deflate.total_out() - before_out) as usize;
        if status == zlib_rs::Status::StreamEnd {
            break;
        }
    }
    out.truncate(written);
    out
}

/// `Account::create()` + credentials + `setUrl(baseUrl)`, on the real HTTP
/// transport.
fn http_account(base_url: &str, user: &str, password: &str) -> Arc<Account> {
    let url = base_url.replacen("http://", &format!("http://{user}:{password}@"), 1);
    let transport = HttpTransport::new(&HttpClientOptions::default()).expect("http client");
    Arc::new(Account::new(
        ServerUrl::parse(&url).expect("url"),
        Arc::new(transport),
    ))
}

#[test]
fn test_get_file_job_decompression_threshold() {
    // generate 50 MiB of easily compressable data, and compress it with
    // the highest possible level to achieve a big enough decompression
    // ratio for Qt's decompression check to kick in
    let decompressed_content = vec![b'A'; 50 * 1024 * 1024];
    // skip the first 4 bytes of compression header to only serve the pure
    // deflate stream
    let compressed_content = Arc::new(zlib_compress(&decompressed_content, 9));
    // ensure the ratio is greater than 40:1 (default for gzip/deflate as
    // per Qt docs)
    let ratio = decompressed_content.len() as f64 / compressed_content.len() as f64;
    println!("Compression ratio is {ratio}");
    assert!(ratio > 40.0);

    // spin up a temprary test-specific HTTP server that serves the
    // compressed data
    let served = compressed_content.clone();
    let http_server = HttpServer::start(vec![(
        "/remote.php/dav/files/admin/someTestFile",
        route(move |_| {
            let mut r = HttpResponse::ok(
                &[
                    ("Content-Encoding", "deflate"),
                    ("OC-ETag", "0123456789abcdef"),
                    ("ETag", "0123456789abcdef"),
                ],
                Vec::new(),
            );
            r.body = served.as_ref().clone();
            r
        }),
    )]);
    let base_url = http_server.base_url();
    println!("Listening on {base_url}");

    let account = http_account(&base_url, "admin", "admin");

    run_event_loop(async {
        {
            println!("Test: with default decompression threshold (20 MiB)");
            let mut received_content: Vec<u8> = Vec::new();
            let job = GetFileJob::new(
                Target::Dav("/someTestFile".to_owned()),
                http::HeaderMap::new(),
                &[],
                0,
            );
            let threshold =
                job.decompression_threshold_base + CUSTOM_DECOMPRESSED_SAFETY_CHECK_THRESHOLD;
            let result = job
                .run(&account, &mut received_content, &mut |_, _| {})
                .await;

            assert!(!result.finished_signal); // the request failed, so the finishedSignal never was emitted
            assert_eq!(result.reply.error, NetworkError::UnknownContentError);
            // the download was aborted below the configured threshold
            assert!((received_content.len() as i64) < threshold);
        }

        {
            println!(
                "Test with decompression threshold set from expected file size (50 MiB + default 20MiB)"
            );
            let mut received_content: Vec<u8> = Vec::new();
            let mut job = GetFileJob::new(
                Target::Dav("/someTestFile".to_owned()),
                http::HeaderMap::new(),
                &[],
                0,
            );
            job.decompression_threshold_base = decompressed_content.len() as i64; // expect a response of 50MiB
            let result = job
                .run(&account, &mut received_content, &mut |_, _| {})
                .await;

            assert!(result.finished_signal);
            assert_eq!(result.reply.error, NetworkError::NoError);
            // the download succeeded
            assert_eq!(received_content.len(), decompressed_content.len());
        }
    });
}

#[test]
fn test_direct_url_credentials() {
    // for this one we need two HTTP servers:
    // - one that acts as a host for direct downloads
    // - and another one that's our pretend-Nextcloud server
    //
    // in any case the requests made to the direct download server should
    // never contain the `Authorization` header

    let direct_download_server = HttpServer::start(vec![
        (
            "/data/directData",
            route(|_| HttpResponse::ok(&[("Content-Type", "text/plain")], "OK directData")),
        ),
        (
            "/data/redirectToOtherUrl",
            route(|_| {
                HttpResponse::with_status(302, &[("Location", "/someOtherPath/redirectTarget")], "")
            }),
        ),
        (
            "/someOtherPath/redirectTarget",
            route(|_| {
                HttpResponse::ok(
                    &[
                        ("Content-Type", "text/plain"),
                        ("OC-ETag", "0123456789abcdef"),
                        ("ETag", "0123456789abcdef"),
                    ],
                    "OK redirectTarget",
                )
            }),
        ),
    ]);
    let direct_download_base_url = direct_download_server.base_url();
    println!("Listening on {direct_download_base_url}");

    let redirect_target = format!("{direct_download_base_url}/someOtherPath/redirectTarget");
    let nextcloud_server = HttpServer::start(vec![
        (
            "/remote.php/dav/files/propagatortest/someFile",
            route(|_| {
                HttpResponse::ok(
                    &[
                        ("Content-Type", "text/plain"),
                        ("OC-ETag", "0123456789abcdef"),
                        ("ETag", "0123456789abcdef"),
                    ],
                    "OK someFile",
                )
            }),
        ),
        (
            "/remote.php/dav/files/propagatortest/redirectedFile",
            route(move |_| {
                HttpResponse::with_status(
                    302,
                    &[
                        ("Content-Type", "text/plain"),
                        ("Location", redirect_target.as_str()),
                    ],
                    "",
                )
            }),
        ),
    ]);
    let nextcloud_base_url = nextcloud_server.base_url();
    println!("Listening on {nextcloud_base_url}");

    // using WebFlowCredentials here to to make sure it works as expected for real
    let account = http_account(&nextcloud_base_url, "propagatortest", "invalid");
    assert_eq!(account.dav_user(), "propagatortest");

    let get = |target: Target| {
        let account = account.clone();
        async move {
            let mut received_content: Vec<u8> = Vec::new();
            let job = GetFileJob::new(target, http::HeaderMap::new(), &[], 0);
            job.run(&account, &mut received_content, &mut |_, _| {})
                .await;
            received_content
        }
    };

    run_event_loop(async {
        println!("Test: direct URL");
        let received = get(Target::Url(format!(
            "{direct_download_base_url}/data/directData"
        )))
        .await;
        // The temporary web servers verify the received headers
        assert_eq!(received, b"OK directData");

        println!("Test: direct URL with redirect");
        let received = get(Target::Url(format!(
            "{direct_download_base_url}/data/redirectToOtherUrl"
        )))
        .await;
        assert_eq!(received, b"OK redirectTarget");

        println!("Test: standard dav path");
        let received = get(Target::Dav("/someFile".to_owned())).await;
        assert_eq!(received, b"OK someFile");

        println!("Test: standard dav path with a redirect to a different origin");
        let received = get(Target::Dav("/redirectedFile".to_owned())).await;
        assert_eq!(received, b"OK redirectTarget");
    });

    // The QVERIFYs of the route handlers.
    let direct_requests = direct_download_server.requests();
    assert_eq!(direct_requests.len(), 4);
    for r in &direct_requests {
        assert!(!r.has_header("Authorization"), "{r:?}");
    }
    let nextcloud_requests = nextcloud_server.requests();
    assert_eq!(nextcloud_requests.len(), 2);
    for r in &nextcloud_requests {
        assert!(r.has_header("Authorization"), "{r:?}");
    }
}
