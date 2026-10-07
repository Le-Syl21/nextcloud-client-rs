/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testconcaturl.cpp (nextcloud/desktop v34.0.5).

use nc_dav::ServerUrl;

type QueryItems = Vec<(String, String)>;

fn make0() -> QueryItems {
    QueryItems::new()
}

fn make1(key: &str, value: &str) -> QueryItems {
    vec![(key.to_owned(), value.to_owned())]
}

fn make2(key1: &str, value1: &str, key2: &str, value2: &str) -> QueryItems {
    vec![
        (key1.to_owned(), value1.to_owned()),
        (key2.to_owned(), value2.to_owned()),
    ]
}

#[test]
fn test_folder() {
    // testFolder_data
    let rows: Vec<(&str, &str, &str, QueryItems, &str)> = vec![
        // Tests about slashes
        ("noslash1", "/baa", "foo", make0(), "/baa/foo"),
        ("noslash2", "", "foo", make0(), "/foo"),
        ("noslash3", "/foo", "", make0(), "/foo"),
        ("noslash4", "", "", make0(), ""),
        ("oneslash1", "/bar/", "foo", make0(), "/bar/foo"),
        ("oneslash2", "/", "foo", make0(), "/foo"),
        ("oneslash3", "/foo", "/", make0(), "/foo/"),
        ("oneslash4", "", "/", make0(), "/"),
        ("twoslash1", "/bar/", "/foo", make0(), "/bar/foo"),
        ("twoslash2", "/", "/foo", make0(), "/foo"),
        ("twoslash3", "/foo/", "/", make0(), "/foo/"),
        ("twoslash4", "/", "/", make0(), "/"),
        // Tests about path encoding
        (
            "encodepath",
            "/a f/b",
            "/a f/c",
            make0(),
            "/a%20f/b/a%20f/c",
        ),
        // Tests about query args
        (
            "query1",
            "/baa",
            "/foo",
            make2("a=a", "b=b", "c", "d"),
            "/baa/foo?a%3Da=b%3Db&c=d",
        ),
        ("query2", "", "", make1("foo", "bar"), "?foo=bar"),
    ];

    // testFolder
    for (row, base, concat, query, expected) in rows {
        let base_url = ServerUrl::parse(&format!("http://example.com{base}")).unwrap();
        let result = base_url.concat_url_path_encoded(concat, &query);
        let expected_full = format!("http://example.com{expected}");
        assert_eq!(result, expected_full, "row {row}");
    }
}
