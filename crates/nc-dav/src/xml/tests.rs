// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! derived tests of the XML parsers (upstream has no dedicated test file;
//! `LsColXMLParser` is exercised through FakeFolder).

use super::*;

const NEXTCLOUD_LISTING: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/admin/Docs%20%26%20Co/</d:href>
  <d:propstat>
   <d:prop>
    <d:resourcetype><d:collection/></d:resourcetype>
    <d:getlastmodified>Wed, 07 Oct 2026 10:21:36 GMT</d:getlastmodified>
    <d:getetag>&quot;6523a1b2c3d4e&quot;</d:getetag>
    <oc:permissions>RGDNVCK</oc:permissions>
    <oc:data-fingerprint/>
    <oc:size>1234</oc:size>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
  <d:propstat>
   <d:prop>
    <d:getcontentlength/>
    <oc:checksums/>
   </d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/admin/Docs%20%26%20Co/a%C3%A9%20%3Cb%3E.txt</d:href>
  <d:propstat>
   <d:prop>
    <d:resourcetype/>
    <d:getcontentlength>42</d:getcontentlength>
    <d:getetag>"abc"</d:getetag>
    <oc:checksums><oc:checksum>SHA1:aa MD5:bb</oc:checksum></oc:checksums>
    <oc:id><![CDATA[00000042oc]]></oc:id>
    <nc:lock-owner-displayname>Tom &amp; Jerry &#233;</nc:lock-owner-displayname>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>
"#;

#[test]
fn derived_lscol_nextcloud_listing() {
    let listing = parse_lscol(
        NEXTCLOUD_LISTING.as_bytes(),
        "/remote.php/dav/files/admin/Docs & Co",
    )
    .unwrap();
    assert_eq!(listing.entries.len(), 2);
    let dir = &listing.entries[0];
    assert_eq!(dir.href, "/remote.php/dav/files/admin/Docs & Co");
    assert_eq!(dir.properties["resourcetype"], "<collection></collection>");
    assert_eq!(dir.properties["getetag"], "\"6523a1b2c3d4e\"");
    assert_eq!(dir.properties["permissions"], "RGDNVCK");
    assert_eq!(dir.properties["data-fingerprint"], "");
    // Properties of the 404 propstat are dropped.
    assert!(!dir.properties.contains_key("getcontentlength"));
    assert_eq!(
        listing.subfolders,
        ["/remote.php/dav/files/admin/Docs & Co/"]
    );

    let file = &listing.entries[1];
    assert_eq!(
        file.href,
        "/remote.php/dav/files/admin/Docs & Co/aé <b>.txt"
    );
    assert_eq!(file.properties["resourcetype"], "");
    assert_eq!(file.properties["getcontentlength"], "42");
    assert_eq!(
        file.properties["checksums"],
        "<checksum>SHA1:aa MD5:bb</checksum>"
    );
    assert_eq!(file.properties["id"], "00000042oc");
    assert_eq!(file.properties["lock-owner-displayname"], "Tom & Jerry é");
}

#[test]
fn derived_lscol_rejects_foreign_href_and_non_multistatus() {
    let err = parse_lscol(NEXTCLOUD_LISTING.as_bytes(), "/remote.php/dav/files/bob").unwrap_err();
    assert!(err.partial.entries.is_empty());
    let err = parse_lscol(b"<html><body>Login</body></html>", "/").unwrap_err();
    assert_eq!(err.message, "no WebDAV response");
    let err = parse_lscol(b"<d:multistatus xmlns:d=\"DAV:\"><d:response>", "/").unwrap_err();
    assert!(err.message.contains("premature"));
}

#[test]
fn derived_lscol_undeclared_d_prefix_is_dav() {
    let xml = br#"<d:multistatus><d:response><d:href>/x/</d:href><d:propstat><d:prop><d:getetag>"e"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let listing = parse_lscol(xml, "/x").unwrap();
    assert_eq!(listing.entries[0].properties["getetag"], "\"e\"");
}

#[test]
fn derived_normalize_href() {
    assert_eq!(normalize_href("/a//b/./c/../d/"), "/a/b/d/");
    assert_eq!(normalize_href("/a%2Fb"), "/a/b");
    assert_eq!(normalize_href("/%E2%82%AC"), "/€");
    assert_eq!(normalize_href("/"), "/");
}

#[test]
fn derived_parse_etag() {
    assert_eq!(parse_etag(b"\"abc\""), b"abc");
    assert_eq!(parse_etag(b"W/\"abc-gzip\""), b"abc");
    assert_eq!(parse_etag(b"abc"), b"abc");
    assert_eq!(parse_etag(b"\""), b"\"");
}

#[test]
fn derived_request_etag_and_propfind() {
    let xml = br#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:response><d:href>/f/</d:href><d:propstat><d:prop><d:getetag>"e1"</d:getetag><oc:permissions>RGD</oc:permissions><oc:share-types><oc:share-type>3</oc:share-type></oc:share-types></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    assert_eq!(parse_request_etag(xml), b"e1");
    let props = parse_propfind(xml).unwrap();
    assert_eq!(props["getetag"], "\"e1\"");
    assert_eq!(props["permissions"], "RGD");
    assert_eq!(props["share-types"], "3");
    assert!(parse_propfind(b"<a>").is_err());
}

#[test]
fn derived_error_message_extraction() {
    let body = br#"<?xml version="1.0" encoding="utf-8"?>
<d:error xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <s:exception>OCA\DAV\Connector\Sabre\Exception\InvalidPath</s:exception>
  <s:message>File name contains at least one invalid character</s:message>
</d:error>"#;
    assert_eq!(
        extract_error_message(body),
        "File name contains at least one invalid character"
    );
    assert_eq!(
        extract_exception(body),
        "OCA\\DAV\\Connector\\Sabre\\Exception\\InvalidPath"
    );
    let no_message = br#"<d:error xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns"><s:exception>X</s:exception><s:message></s:message></d:error>"#;
    assert_eq!(extract_error_message(no_message), "X");
    assert_eq!(extract_error_message(b"not xml"), "");
}
