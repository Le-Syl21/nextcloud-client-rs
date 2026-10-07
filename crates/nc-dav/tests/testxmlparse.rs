/*
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2015 ownCloud, Inc.
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testxmlparse.cpp (nextcloud/desktop v34.0.5).
//
// Upstream connects slots to the `LsColXMLParser` signals; here the parse
// result is mapped to the same observable state: `directoryListingIterated`
// is emitted while parsing (so also before a failure, kept in
// `LsColError::partial`), `directoryListingSubfolders` and
// `finishedWithoutError` only on success. `sizes` is the `fileInfo`
// out-parameter (`LsColListing::folder_infos`).

use std::collections::HashMap;

use nc_dav::xml::{ExtraFolderInfo, parse_lscol};

/// The members of upstream's `TestXmlParse` (reset in `init()`).
#[derive(Default)]
struct TestXmlParse {
    success: bool,
    subdirs: Vec<String>,
    items: Vec<String>,
}

impl TestXmlParse {
    /// `parser.parse(testXml, &sizes, expectedPath)` with the slots connected.
    fn parse(
        &mut self,
        xml: &str,
        sizes: &mut HashMap<String, ExtraFolderInfo>,
        expected_path: &str,
    ) -> bool {
        match parse_lscol(xml.as_bytes(), expected_path) {
            Ok(listing) => {
                // slotDirectoryListingIterated
                self.items
                    .extend(listing.entries.into_iter().map(|e| e.href));
                sizes.extend(listing.folder_infos);
                // slotDirectoryListingSubFolders
                self.subdirs.extend(listing.subfolders);
                // slotFinishedSuccessfully
                self.success = true;
                true
            }
            Err(e) => {
                self.items
                    .extend(e.partial.entries.into_iter().map(|e| e.href));
                sizes.extend(e.partial.folder_infos);
                false
            }
        }
    }

    fn items_contains(&self, s: &str) -> bool {
        self.items.iter().any(|i| i == s)
    }

    fn subdirs_contains(&self, s: &str) -> bool {
        self.subdirs.iter().any(|i| i == s)
    }
}

#[test]
fn test_parser1() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(t.parse(
        TEST_PARSER1_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    ));

    assert!(t.success);
    assert_eq!(sizes.len(), 1); // Quota info in the XML

    assert!(t.items_contains("/oc/remote.php/dav/sharefolder/quitte.pdf"));
    assert!(t.items_contains("/oc/remote.php/dav/sharefolder"));
    assert!(t.items.len() == 2);

    assert!(t.subdirs_contains("/oc/remote.php/dav/sharefolder/"));
    assert!(t.subdirs.len() == 1);
}

#[test]
fn test_parser_broken_xml() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_BROKEN_XML_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    )); // verify false

    assert!(!t.success);
    assert!(sizes.is_empty()); // No quota info in the XML

    assert!(t.items.is_empty()); // FIXME: We should change the parser to not emit during parsing but at the end

    assert!(t.subdirs.is_empty());
}

#[test]
fn test_parser_empty_xml_no_dav() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_EMPTY_XML_NO_DAV_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    )); // verify false

    assert!(!t.success);
    assert!(sizes.is_empty()); // No quota info in the XML

    assert!(t.items.is_empty()); // FIXME: We should change the parser to not emit during parsing but at the end
    assert!(t.subdirs.is_empty());
}

#[test]
fn test_parser_empty_xml() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_EMPTY_XML_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    )); // verify false

    assert!(!t.success);
    assert!(sizes.is_empty()); // No quota info in the XML

    assert!(t.items.is_empty()); // FIXME: We should change the parser to not emit during parsing but at the end
    assert!(t.subdirs.is_empty());
}

#[test]
fn test_parser_truncated_xml() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_TRUNCATED_XML_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    ));
    assert!(!t.success);
}

#[test]
fn test_parser_bogfus_href1() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_BOGFUS_HREF1_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    ));
    assert!(!t.success);
}

#[test]
fn test_parser_bogfus_href2() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_BOGFUS_HREF2_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    ));
    assert!(!t.success);
}

#[test]
fn test_parser_denormalized_path() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(t.parse(
        TEST_PARSER_DENORMALIZED_PATH_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    ));

    assert!(t.success);
    assert_eq!(sizes.len(), 1); // Quota info in the XML

    assert!(t.items_contains("/oc/remote.php/dav/sharefolder/quitte.pdf"));
    assert!(t.items_contains("/oc/remote.php/dav/sharefolder"));
    assert!(t.items.len() == 2);

    assert!(t.subdirs_contains("/oc/remote.php/dav/sharefolder/"));
    assert!(t.subdirs.len() == 1);
}

#[test]
fn test_parser_denormalized_path_outside_namespace() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(!t.parse(
        TEST_PARSER_DENORMALIZED_PATH_OUTSIDE_NAMESPACE_XML,
        &mut sizes,
        "/oc/remote.php/dav/sharefolder"
    ));

    assert!(!t.success);
}

#[test]
fn test_href_url_encoding() {
    let mut t = TestXmlParse::default();
    let mut sizes = HashMap::new();
    assert!(t.parse(TEST_HREF_URL_ENCODING_XML, &mut sizes, "/ä"));
    assert!(t.success);

    assert!(t.items_contains("/ä/ä.pdf"));
    assert!(t.items_contains("/ä"));
    assert!(t.items.len() == 2);

    assert!(t.subdirs_contains("/ä"));
    assert!(t.subdirs.len() == 1);
}

// testParser1
const TEST_PARSER1_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/quitte.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);

// testParserBrokenXml
const TEST_PARSER_BROKEN_XML_XML: &str = concat!(
    "X<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/quitte.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);

// testParserEmptyXmlNoDav
const TEST_PARSER_EMPTY_XML_NO_DAV_XML: &str = "<html><body>I am under construction</body></html>";

// testParserEmptyXml
const TEST_PARSER_EMPTY_XML_XML: &str = "";

// testParserTruncatedXml
const TEST_PARSER_TRUNCATED_XML_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>", // no proper end here
);

// testParserBogfusHref1
const TEST_PARSER_BOGFUS_HREF1_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>http://127.0.0.1:81/oc/remote.php/dav/sharefolder/</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>http://127.0.0.1:81/oc/remote.php/dav/sharefolder/quitte.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);

// testParserBogfusHref2
const TEST_PARSER_BOGFUS_HREF2_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/sharefolder</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>/sharefolder/quitte.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);

// testParserDenormalizedPath
const TEST_PARSER_DENORMALIZED_PATH_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/../sharefolder/quitte.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);

// testParserDenormalizedPathOutsideNamespace
const TEST_PARSER_DENORMALIZED_PATH_OUTSIDE_NAMESPACE_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>/oc/remote.php/dav/sharefolder/../quitte.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);

// testHrefUrlEncoding
const TEST_HREF_URL_ENCODING_XML: &str = concat!(
    "<?xml version='1.0' encoding='utf-8'?>",
    "<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">",
    "<d:response>",
    "<d:href>/%C3%A4</d:href>", // a-umlaut utf8
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004213ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVCK</oc:permissions>",
    "<oc:size>121780</oc:size>",
    "<d:getetag>\"5527beb0400b0\"</d:getetag>",
    "<d:resourcetype>",
    "<d:collection/>",
    "</d:resourcetype>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<d:getcontentlength/>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "<d:response>",
    "<d:href>/%C3%A4/%C3%A4.pdf</d:href>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:id>00004215ocobzus5kn6s</oc:id>",
    "<oc:permissions>RDNVW</oc:permissions>",
    "<d:getetag>\"2fa2f0d9ed49ea0c3e409d49e652dea0\"</d:getetag>",
    "<d:resourcetype/>",
    "<d:getlastmodified>Fri, 06 Feb 2015 13:49:55 GMT</d:getlastmodified>",
    "<d:getcontentlength>121780</d:getcontentlength>",
    "</d:prop>",
    "<d:status>HTTP/1.1 200 OK</d:status>",
    "</d:propstat>",
    "<d:propstat>",
    "<d:prop>",
    "<oc:downloadURL/>",
    "<oc:dDC/>",
    "</d:prop>",
    "<d:status>HTTP/1.1 404 Not Found</d:status>",
    "</d:propstat>",
    "</d:response>",
    "</d:multistatus>",
);
