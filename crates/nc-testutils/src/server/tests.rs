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

use super::*;
use crate::{FileModifier, block_on};
use nc_dav::method;

fn req(m: http::Method, path: &str) -> http::request::Builder {
    http::Request::builder()
        .method(m)
        .uri(format!("{ROOT_URL2}{path}"))
}

fn send(server: &FakeServer, r: Request) -> Response {
    block_on(server.send(r)).unwrap()
}

#[test]
fn file_path_from_url_prefixes() {
    let p = |s: &str| file_path_from_url(&s.parse().unwrap());
    assert_eq!(
        p("owncloud://somehost/owncloud/remote.php/dav/files/admin/A/a1").as_deref(),
        Some("A/a1")
    );
    assert_eq!(
        p("owncloud://somehost/owncloud/remote.php/dav/files/admin/").as_deref(),
        Some("")
    );
    assert_eq!(
        p("owncloud://somehost/owncloud/remote.php/dav/uploads/admin/t/1").as_deref(),
        Some("t/1")
    );
    assert_eq!(
        p("owncloud://somehost/owncloud/remote.php/dav/x").as_deref(),
        Some("x")
    );
    assert_eq!(
        p("owncloud://somehost/owncloud/remote.php/dav/files/admin/a%20b").as_deref(),
        Some("a b")
    );
    assert_eq!(p("owncloud://somehost/ocs/v1.php"), None);
}

#[test]
fn propfind_root_lists_children() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let resp = send(
        &server,
        req(method::propfind(), "")
            .header("Depth", "1")
            .body(Body::empty())
            .unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
    assert_eq!(
        resp.headers()["Content-Type"],
        "application/xml; charset=utf-8"
    );
    assert!(resp.headers().contains_key("Date"));
    let body = std::str::from_utf8(resp.body().as_bytes().unwrap()).unwrap();
    assert!(
        body.starts_with(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><d:multistatus xmlns:d=\"DAV:\""
        )
    );
    assert_eq!(body.matches("<d:response>").count(), 5);
    assert!(body.contains("<d:href>/owncloud/remote.php/dav/files/admin/</d:href>"));
    assert!(body.contains("<d:href>/owncloud/remote.php/dav/files/admin/A/</d:href>"));
    // The root reports the sum of its direct children (all dirs: 0).
    assert!(body.contains("<oc:size>0</oc:size>"));
    // shared folder
    assert!(body.contains("<oc:permissions>GSRDNVCKW</oc:permissions>"));
    // QXmlStreamWriter escapes quotes in text too
    assert!(body.contains("&quot;key&quot;:&quot;download&quot;,&quot;value&quot;:true"));
    assert!(body.contains("<oc:share-permissions>31</oc:share-permissions>"));
    let etag = server.state().remote_root.find("A").unwrap().etag.clone();
    assert!(body.contains(&format!("<d:getetag>&quot;{etag}&quot;</d:getetag>")));
}

#[test]
fn propfind_file_and_dir_props() {
    let mut root = FileInfo::A12_B12_C12_S12();
    {
        let a1 = root.find_mut("A/a1").unwrap();
        a1.checksums = "SHA1:abc".into();
        a1.permissions =
            nc_journal::remote_permissions::RemotePermissions::from_server_string("GWD");
        a1.extra_dav_properties = "<oc:custom>x</oc:custom>".into();
        a1.last_modified = crate::util::from_secs(1_791_368_496);
    }
    let fileid = root.find("A/a1").unwrap().numeric_file_id();
    let server = FakeServer::new(root);
    let resp = send(
        &server,
        req(method::propfind(), "A").body(Body::empty()).unwrap(),
    );
    let body = String::from_utf8(resp.body().as_bytes().unwrap().to_vec()).unwrap();
    assert_eq!(body.matches("<d:response>").count(), 3);
    assert!(body.contains("<oc:size>8</oc:size>"));
    assert!(body.contains("<d:href>/owncloud/remote.php/dav/files/admin/A/a1/</d:href>"));
    assert!(body.contains("<d:resourcetype/><d:getlastmodified>Wed, 07 Oct 2026 10:21:36 GMT</d:getlastmodified><d:getcontentlength>4</d:getcontentlength>"));
    assert!(body.contains("<oc:permissions>GWD</oc:permissions>"));
    assert!(body.contains("<oc:checksums>SHA1:abc</oc:checksums>"));
    assert!(body.contains(&format!("<oc:fileid>{fileid}</oc:fileid>")));
    assert!(body.contains("<nc:metadata-files-live-photo>0</nc:metadata-files-live-photo><oc:custom>x</oc:custom></d:prop>"));

    let missing = send(
        &server,
        req(method::propfind(), "nope").body(Body::empty()).unwrap(),
    );
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[test]
fn propfind_href_percent_encoding() {
    let mut root = FileInfo::default();
    root.mkdir("a b");
    root.insert("a b/é&.txt", 3, b'W');
    let server = FakeServer::new(root);
    let resp = send(
        &server,
        req(method::propfind(), "a%20b")
            .body(Body::empty())
            .unwrap(),
    );
    let body = String::from_utf8(resp.body().as_bytes().unwrap().to_vec()).unwrap();
    assert!(body.contains("<d:href>/owncloud/remote.php/dav/files/admin/a%20b/</d:href>"));
    assert!(
        body.contains("<d:href>/owncloud/remote.php/dav/files/admin/a%20b/%C3%A9%26.txt/</d:href>")
    );
}

#[test]
fn get_returns_content() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let resp = send(
        &server,
        req(method::GET, "B/b1").body(Body::empty()).unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.body().as_bytes().unwrap().as_ref(), &[b'W'; 16]);
    assert_eq!(resp.headers()["Content-Length"], "16");
    let state = server.state();
    let b1 = state.remote_root.find("B/b1").unwrap();
    assert_eq!(resp.headers()["ETag"], b1.etag.as_str());
    assert_eq!(resp.headers()["OC-ETag"], b1.etag.as_str());
    assert_eq!(resp.headers()["OC-FileId"], b1.file_id.as_str());
    drop(state);
    let missing = send(
        &server,
        req(method::GET, "B/nope").body(Body::empty()).unwrap(),
    );
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[test]
fn put_creates_and_updates() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let root_etag = server.state().remote_root.etag.clone();
    let a_etag = server.state().remote_root.find("A").unwrap().etag.clone();
    let resp = send(
        &server,
        req(method::PUT, "A/new")
            .header("X-OC-Mtime", "1791368496")
            .header("OC-Checksum", "SHA1:whatever")
            .body(Body::from(Bytes::from_static(b"QQQ")))
            .unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["X-OC-MTime"], "accepted");
    {
        let state = server.state();
        let new = state.remote_root.find("A/new").unwrap();
        assert_eq!((new.size, new.content_char, new.is_dir), (3, b'Q', false));
        assert_eq!(
            crate::util::secs_since_epoch(new.last_modified),
            1_791_368_496
        );
        assert_eq!(resp.headers()["ETag"], new.etag.as_str());
        assert_eq!(resp.headers()["OC-FileID"], new.file_id.as_str());
        assert_ne!(state.remote_root.etag, root_etag);
        assert_ne!(state.remote_root.find("A").unwrap().etag, a_etag);
    }
    // update keeps the file id
    let id = server
        .state()
        .remote_root
        .find("A/a1")
        .unwrap()
        .file_id
        .clone();
    let resp = send(
        &server,
        req(method::PUT, "A/a1")
            .header("X-OC-Mtime", "5")
            .body(Body::from(Bytes::from_static(b"ZZ")))
            .unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::OK);
    let state = server.state();
    let a1 = state.remote_root.find("A/a1").unwrap();
    assert_eq!(
        (a1.size, a1.content_char, a1.file_id.as_str()),
        (2, b'Z', id.as_str())
    );
}

#[test]
fn put_errors() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let bad_mtime = send(
        &server,
        req(method::PUT, "A/x")
            .header("X-OC-Mtime", "0")
            .body(Body::from(Bytes::from_static(b"W")))
            .unwrap(),
    );
    assert_eq!(bad_mtime.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let no_parent = send(
        &server,
        req(method::PUT, "Z/x")
            .header("X-OC-Mtime", "1")
            .body(Body::from(Bytes::from_static(b"W")))
            .unwrap(),
    );
    assert_eq!(no_parent.status(), StatusCode::PRECONDITION_FAILED);
}

#[test]
fn mkcol_delete_move() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let resp = send(
        &server,
        req(method::mkcol(), "D").body(Body::empty()).unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()["OC-FileId"],
        server
            .state()
            .remote_root
            .find("D")
            .unwrap()
            .file_id
            .as_str()
    );
    assert_eq!(
        send(
            &server,
            req(method::mkcol(), "X/Y").body(Body::empty()).unwrap()
        )
        .status(),
        StatusCode::CONFLICT
    );

    let resp = send(
        &server,
        req(method::move_(), "A/a1")
            .header("Destination", format!("{ROOT_URL2}D/moved%20a1"))
            .body(Body::empty())
            .unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(server.state().remote_root.find("A/a1").is_none());
    assert_eq!(
        server
            .state()
            .remote_root
            .find("D/moved a1")
            .unwrap()
            .path(),
        "D/moved a1"
    );

    assert_eq!(
        send(
            &server,
            req(method::DELETE, "B").body(Body::empty()).unwrap()
        )
        .status(),
        StatusCode::NO_CONTENT
    );
    assert!(server.state().remote_root.find("B").is_none());
}

#[test]
fn chunked_upload_move() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let up = |path: &str| http::Request::builder().uri(format!("{UPLOAD_URL}{path}"));
    assert_eq!(
        send(
            &server,
            up("t1")
                .method(method::mkcol())
                .body(Body::empty())
                .unwrap()
        )
        .status(),
        StatusCode::CREATED
    );
    for chunk in ["00001", "00002"] {
        let r = up(&format!("t1/{chunk}"))
            .method(method::PUT)
            .body(Body::from(Bytes::from_static(b"CCCC")))
            .unwrap();
        assert_eq!(send(&server, r).status(), StatusCode::OK);
    }
    let r = up("t1/.file")
        .method(method::move_())
        .header("Destination", format!("{ROOT_URL2}A/big"))
        .header("X-OC-Mtime", "100")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&server, r).status(), StatusCode::CREATED);
    let big = server.state().remote_root.find("A/big").unwrap().clone();
    assert_eq!((big.size, big.content_char), (8, b'C'));

    // Overwrite with a wrong If precondition: 412.
    let dest = format!("{ROOT_URL2}A/big");
    let r = up("t1/.file")
        .method(method::move_())
        .header("Destination", dest.as_str())
        .header("If", format!("<{dest}> ([\"wrong\"])"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&server, r).status(), StatusCode::PRECONDITION_FAILED);
}

#[test]
fn lock_reply() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    let resp = send(
        &server,
        req(method::lock(), "A/a1").body(Body::empty()).unwrap(),
    );
    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
    let body = String::from_utf8(resp.body().as_bytes().unwrap().to_vec()).unwrap();
    assert!(body.contains("<nc:lock>1</nc:lock>"));
    assert!(body.contains("<nc:lock-owner>admin</nc:lock-owner>"));
    let resp = send(
        &server,
        req(method::unlock(), "A/a1").body(Body::empty()).unwrap(),
    );
    assert!(
        String::from_utf8(resp.body().as_bytes().unwrap().to_vec())
            .unwrap()
            .contains("<nc:lock>0</nc:lock>")
    );
}

#[test]
fn error_paths_and_override() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    server.state().error_paths.insert("A/a1".into(), 403);
    assert_eq!(
        send(
            &server,
            req(method::GET, "A/a1").body(Body::empty()).unwrap()
        )
        .status(),
        StatusCode::FORBIDDEN
    );

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = counter.clone();
    server.set_override(move |request, state| {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(request.headers().contains_key("X-Request-ID"));
        if request.uri().path().ends_with("/B/b1") {
            state.remote_root.append_byte("B/b1");
            return Some(FakeReply::Response(payload_reply(Bytes::from_static(
                b"over",
            ))));
        }
        if request.uri().path().ends_with("/hang") {
            return Some(FakeReply::Hang);
        }
        None
    });
    let resp = send(
        &server,
        req(method::GET, "B/b1").body(Body::empty()).unwrap(),
    );
    assert_eq!(resp.body().as_bytes().unwrap().as_ref(), b"over");
    assert_eq!(server.state().remote_root.find("B/b1").unwrap().size, 17);
    // falls through to the default handler
    assert_eq!(
        send(
            &server,
            req(method::GET, "C/c1").body(Body::empty()).unwrap()
        )
        .status(),
        StatusCode::OK
    );
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2);

    // a hanging reply never completes
    let mut fut = server.send(req(method::GET, "hang").body(Body::empty()).unwrap());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
}

#[test]
fn get_with_data_range() {
    let root = FileInfo::A12_B12_C12_S12();
    let r = req(method::GET, "A/a1")
        .header("Range", "bytes=2-4")
        .body(Body::empty())
        .unwrap();
    let resp = get_with_data_reply(&root, b"0123456789", &r);
    assert_eq!(resp.body().as_bytes().unwrap().as_ref(), b"234");
}

#[test]
fn unsupported_methods() {
    let server = FakeServer::new(FileInfo::A12_B12_C12_S12());
    assert_eq!(
        send(&server, req(method::POST, "").body(Body::empty()).unwrap()).status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(
        send(
            &server,
            req(method::copy(), "A").body(Body::empty()).unwrap()
        )
        .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let ocs = http::Request::builder()
        .uri("owncloud://somehost/ocs/v2.php/cloud/capabilities")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&server, ocs).status(), StatusCode::NOT_FOUND);
}
