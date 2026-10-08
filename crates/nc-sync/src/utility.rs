// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2008-2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the parts of upstream `src/common/utility.cpp` (LGPL-2.1-or-later
// upstream) the sync engine uses, plus the Qt conversion rules the engine
// relies on (nextcloud/desktop v34.0.5).

//! Small helpers: Qt number parsing rules, dates, conflict file names,
//! human-readable sizes and durations.

use jiff::tz::TimeZone;

/// `QString::operator<`: compares UTF-16 code units.
pub fn qstring_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `QString::toInt()`: 0 when not a valid 32-bit integer.
pub fn qstring_to_int(s: &str) -> i64 {
    s.trim().parse::<i32>().map(i64::from).unwrap_or(0)
}

/// `QString::toInt(&ok)`.
pub fn qstring_to_int_ok(s: &str) -> Option<i64> {
    s.trim().parse::<i32>().ok().map(i64::from)
}

/// `QString::toLongLong()`: 0 when not a valid 64-bit integer.
pub fn qstring_to_long_long(s: &str) -> i64 {
    s.trim().parse::<i64>().unwrap_or(0)
}

/// `QString::toLongLong(&ok)`.
pub fn qstring_to_long_long_ok(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

/// `QString::toDouble(&ok)` cast to `int64_t` (quota values may come as
/// `2.58440798353E+12`).
pub fn qstring_to_double_i64(s: &str) -> Option<i64> {
    s.trim().parse::<f64>().ok().map(|d| d as i64)
}

/// `QDateTime::fromString(value, Qt::RFC2822Date).toSecsSinceEpoch()`.
pub fn parse_rfc2822(value: &str) -> Option<i64> {
    let v = value.trim();
    if let Ok(z) = jiff::fmt::rfc2822::parse(v) {
        return Some(z.timestamp().as_second());
    }
    // Qt also accepts the date without the day name.
    httpdate::parse_http_date(v)
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// `QDateTime::currentSecsSinceEpoch()`.
pub fn current_secs_since_epoch() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// `QDateTime::fromSecsSinceEpoch(t).toString("yyyy-MM-dd hhmmss")` in local time.
fn local_conflict_timestamp(t: i64) -> String {
    let ts = jiff::Timestamp::from_second(t).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    ts.to_zoned(TimeZone::system())
        .strftime("%Y-%m-%d %H%M%S")
        .to_string()
}

/// `Utility::sanitizeForFileName`: drops `/?<>\:*|"`, control and format
/// characters.
pub fn sanitize_for_file_name(name: &str) -> String {
    const INVALID: &str = r#"/?<>\:*|""#;
    name.chars()
        .filter(|c| !INVALID.contains(*c) && !c.is_control() && !is_format_char(*c))
        .collect()
}

/// Unicode general category Cf (`QChar::Other_Format`), the common ones.
fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Position before which a conflict tag is inserted: before the extension,
/// or at the end (`foo/.hidden`, `foo.bar/file`).
fn conflict_tag_position(fname: &str) -> usize {
    let dot = fname.rfind('.').map_or(-1, |d| d as i64);
    let slash = fname.rfind('/').map_or(-1, |s| s as i64);
    if dot <= slash + 1 {
        fname.len()
    } else {
        dot as usize
    }
}

/// `Utility::makeConflictFileName(fn, dt, user)`.
pub fn make_conflict_file_name(fname: &str, mtime: i64, user: &str) -> String {
    let pos = conflict_tag_position(fname);
    let mut marker = String::from(" (conflicted copy ");
    if !user.is_empty() {
        // Don't allow parens in the user name, to ensure
        // we can find the beginning and end of the conflict tag.
        let user_name = sanitize_for_file_name(user).replace(['(', ')'], "_");
        marker.push_str(&user_name);
        marker.push(' ');
    }
    marker.push_str(&local_conflict_timestamp(mtime));
    marker.push(')');
    let mut out = fname.to_owned();
    out.insert_str(pos, &marker);
    out
}

/// `Utility::makeCaseClashConflictFileName`.
pub fn make_case_clash_conflict_file_name(fname: &str, mtime: i64) -> String {
    let pos = conflict_tag_position(fname);
    let marker = format!(" (case clash from {})", local_conflict_timestamp(mtime));
    let mut out = fname.to_owned();
    out.insert_str(pos, &marker);
    out
}

/// `Utility::normalizeEtag`.
pub fn normalize_etag(etag: &[u8]) -> Vec<u8> {
    let mut e = etag.to_vec();
    // strip "XXXX-gzip"
    if e.starts_with(b"\"") && e.ends_with(b"-gzip\"") {
        e.truncate(e.len() - 6);
        e.remove(0);
    }
    // strip trailing -gzip
    if e.ends_with(b"-gzip") {
        e.truncate(e.len() - 5);
    }
    // strip normal quotes
    if e.len() >= 2 && e.starts_with(b"\"") && e.ends_with(b"\"") {
        e.truncate(e.len() - 1);
        e.remove(0);
    } else if e == b"\"" {
        // QByteArray("\"").chop(1).remove(0, 1) leaves an empty array.
        e.clear();
    }
    e
}

/// `Utility::octetsToString` (with the default C locale, no translation).
pub fn octets_to_string(octets: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let mut data = octets as f64;
    let mut unit = "B";
    let mut show_decimals = true;
    if data >= TB {
        unit = "TB";
        data /= TB;
        show_decimals = false;
    } else if data >= GB {
        unit = "GB";
        data /= GB;
        show_decimals = false;
    } else if data >= MB {
        unit = "MB";
        data /= MB;
        show_decimals = false;
    } else if data >= KB {
        unit = "KB";
        data /= KB;
    }
    if data > 9.95 {
        show_decimals = true;
    }
    if show_decimals {
        format!("{} {unit}", data.round() as i64)
    } else {
        // 'g' with 2 significant digits.
        let s = format!("{:.1}", data);
        let s = s.trim_end_matches('0').trim_end_matches('.').to_owned();
        format!("{s} {unit}")
    }
}

/// The `periods` table of `utility.cpp`.
const PERIODS: &[(&str, u64)] = &[
    ("year(s)", 365 * 24 * 3600 * 1000),
    ("month(s)", 30 * 24 * 3600 * 1000),
    ("day(s)", 24 * 3600 * 1000),
    ("hour(s)", 3600 * 1000),
    ("minute(s)", 60 * 1000),
    ("second(s)", 1000),
];

/// `Utility::durationToDescriptiveString2` (untranslated plural forms):
/// the two most significant units.
pub fn duration_to_descriptive_string2(msecs: u64) -> String {
    let mut p = 0;
    while p + 1 < PERIODS.len() && msecs < PERIODS[p].1 {
        p += 1;
    }
    let first_part = format!("{} {}", msecs / PERIODS[p].1, PERIODS[p].0);
    if p + 1 >= PERIODS.len() {
        return first_part;
    }
    let second_part_num = ((msecs % PERIODS[p].1) as f64 / PERIODS[p + 1].1 as f64).round() as u64;
    if second_part_num == 0 {
        return first_part;
    }
    format!("{first_part} {second_part_num} {}", PERIODS[p + 1].0)
}

/// `Utility::durationToDescriptiveString1` (untranslated plural forms).
pub fn duration_to_descriptive_string1(msecs: u64) -> String {
    let mut p = 0;
    while p + 1 < PERIODS.len() && msecs < PERIODS[p].1 {
        p += 1;
    }
    let amount = (msecs as f64 / PERIODS[p].1 as f64).round() as u64;
    format!("{amount} {}", PERIODS[p].0)
}

/// `Utility::fullRemotePathToRemoteSyncRootRelative`.
pub fn full_remote_path_to_remote_sync_root_relative(
    full_remote_path: &str,
    remote_sync_root: &str,
) -> String {
    use nc_journal::utility::{no_leading_slash_path, no_trailing_slash_path, trailing_slash_path};
    let root = trailing_slash_path(no_leading_slash_path(remote_sync_root));
    let full = no_leading_slash_path(full_remote_path);
    if root == "/" {
        return no_leading_slash_path(no_trailing_slash_path(full_remote_path)).to_owned();
    }
    let Some(rel) = full.strip_prefix(root.as_str()) else {
        return full_remote_path.to_owned();
    };
    no_leading_slash_path(no_trailing_slash_path(rel)).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_conflict_names() {
        let n = make_conflict_file_name("a/b.txt", 0, "");
        assert!(n.starts_with("a/b (conflicted copy "));
        assert!(n.ends_with(").txt"));
        let n = make_conflict_file_name("a/.hidden", 0, "Jo(h)n/x");
        assert!(n.starts_with("a/.hidden (conflicted copy Jo_h_nx "));
        let n = make_conflict_file_name("foo.bar/file", 0, "");
        assert!(n.starts_with("foo.bar/file (conflicted copy "));
        let n = make_conflict_file_name("noext", 0, "");
        assert!(n.starts_with("noext (conflicted copy "));
        let n = make_case_clash_conflict_file_name("A/x.y.z", 0);
        assert!(n.starts_with("A/x.y (case clash from "));
        assert!(n.ends_with(").z"));
    }

    #[test]
    fn derived_qt_conversions() {
        assert_eq!(qstring_to_int("123"), 123);
        assert_eq!(qstring_to_int("3000000000"), 0);
        assert_eq!(
            qstring_to_double_i64("2.58440798353E+12"),
            Some(2_584_407_983_530)
        );
        assert_eq!(
            parse_rfc2822("Wed, 07 Oct 2026 10:21:36 +0000"),
            Some(1_791_368_496)
        );
        assert_eq!(normalize_etag(b"\"abc-gzip\""), b"abc");
        assert_eq!(normalize_etag(b"\"abc\""), b"abc");
        assert_eq!(normalize_etag(b"abc"), b"abc");
        assert_eq!(octets_to_string(500), "500 B");
        assert_eq!(octets_to_string(2048), "2 KB");
        assert_eq!(octets_to_string(1536 * 1024), "1.5 MB");
        assert_eq!(duration_to_descriptive_string1(25_000), "25 second(s)");
        assert_eq!(duration_to_descriptive_string1(7_200_000), "2 hour(s)");
        assert_eq!(
            full_remote_path_to_remote_sync_root_relative("/sub/A/b/", "/sub"),
            "A/b"
        );
        assert_eq!(
            full_remote_path_to_remote_sync_root_relative("/A/b", "/"),
            "A/b"
        );
    }

    // Ports of upstream test/testutility.cpp (CC0-1.0,
    // SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors,
    // 2014 ownCloud GmbH): the functions whose subject lives here.

    #[test]
    fn test_octets_to_string() {
        assert_eq!(octets_to_string(999), "999 B");
        assert_eq!(octets_to_string(1024), "1 KB");
        assert_eq!(octets_to_string(1110), "1 KB");
        assert_eq!(octets_to_string(1364), "1 KB");

        assert_eq!(octets_to_string(9110), "9 KB");
        assert_eq!(octets_to_string(9910), "10 KB");
        assert_eq!(octets_to_string(9999), "10 KB");
        assert_eq!(octets_to_string(10240), "10 KB");

        assert_eq!(octets_to_string(123456), "121 KB");
        assert_eq!(octets_to_string(1234567), "1.2 MB");
        assert_eq!(octets_to_string(12345678), "12 MB");
        assert_eq!(octets_to_string(123456789), "118 MB");
        assert_eq!(octets_to_string(1000 * 1000 * 1000 * 5), "4.7 GB");

        assert_eq!(octets_to_string(1), "1 B");
        assert_eq!(octets_to_string(2), "2 B");
        assert_eq!(octets_to_string(1024), "1 KB");
        assert_eq!(octets_to_string(1024 * 1024), "1 MB");
        assert_eq!(octets_to_string(1024 * 1024 * 1024), "1 GB");
        assert_eq!(octets_to_string(1024 * 1024 * 1024 * 1024), "1 TB");
        assert_eq!(octets_to_string(1024 * 1024 * 1024 * 1024 * 5), "5 TB");
    }

    #[test]
    fn test_duration_to_descriptive_string() {
        let sec: u64 = 1000;
        let hour: u64 = 3600 * sec;

        let current = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
        let msecs_to = |later: jiff::Zoned| {
            (later.timestamp().as_millisecond() - current.timestamp().as_millisecond()) as u64
        };
        let in_4y5m2d23h = || {
            msecs_to(
                current
                    .checked_add(jiff::Span::new().years(4).months(5).days(2))
                    .unwrap()
                    .checked_add(jiff::Span::new().hours(23))
                    .unwrap(),
            )
        };
        let in_2d23h = || {
            msecs_to(
                current
                    .checked_add(jiff::Span::new().days(2))
                    .unwrap()
                    .checked_add(jiff::Span::new().hours(23))
                    .unwrap(),
            )
        };

        assert_eq!(duration_to_descriptive_string2(0), "0 second(s)");
        assert_eq!(duration_to_descriptive_string2(5), "0 second(s)");
        assert_eq!(duration_to_descriptive_string2(1000), "1 second(s)");
        assert_eq!(duration_to_descriptive_string2(1005), "1 second(s)");
        assert_eq!(duration_to_descriptive_string2(56123), "56 second(s)");
        assert_eq!(
            duration_to_descriptive_string2(90 * sec),
            "1 minute(s) 30 second(s)"
        );
        assert_eq!(duration_to_descriptive_string2(3 * hour), "3 hour(s)");
        assert_eq!(
            duration_to_descriptive_string2(3 * hour + 20 * sec),
            "3 hour(s)"
        );
        assert_eq!(
            duration_to_descriptive_string2(3 * hour + 70 * sec),
            "3 hour(s) 1 minute(s)"
        );
        assert_eq!(
            duration_to_descriptive_string2(3 * hour + 100 * sec),
            "3 hour(s) 2 minute(s)"
        );
        assert_eq!(
            duration_to_descriptive_string2(in_4y5m2d23h()),
            "4 year(s) 5 month(s)"
        );
        assert_eq!(
            duration_to_descriptive_string2(in_2d23h()),
            "2 day(s) 23 hour(s)"
        );

        assert_eq!(duration_to_descriptive_string1(0), "0 second(s)");
        assert_eq!(duration_to_descriptive_string1(5), "0 second(s)");
        assert_eq!(duration_to_descriptive_string1(1000), "1 second(s)");
        assert_eq!(duration_to_descriptive_string1(1005), "1 second(s)");
        assert_eq!(duration_to_descriptive_string1(56123), "56 second(s)");
        assert_eq!(duration_to_descriptive_string1(90 * sec), "2 minute(s)");
        assert_eq!(duration_to_descriptive_string1(3 * hour), "3 hour(s)");
        assert_eq!(
            duration_to_descriptive_string1(3 * hour + 20 * sec),
            "3 hour(s)"
        );
        assert_eq!(
            duration_to_descriptive_string1(3 * hour + 70 * sec),
            "3 hour(s)"
        );
        assert_eq!(
            duration_to_descriptive_string1(3 * hour + 100 * sec),
            "3 hour(s)"
        );
        assert_eq!(duration_to_descriptive_string1(in_4y5m2d23h()), "4 year(s)");
        assert_eq!(duration_to_descriptive_string1(in_2d23h()), "3 day(s)");
    }

    #[test]
    fn test_sanitize_for_file_name() {
        // _data rows (all named "")
        assert_eq!(sanitize_for_file_name("foobar"), "foobar");
        assert_eq!(
            sanitize_for_file_name("a/b?c<d>e\\f:g*h|i\"j"),
            "abcdefghij"
        );
        assert_eq!(
            sanitize_for_file_name("a\u{01} b\u{1f} c\u{80} d\u{9f}"),
            "a b c d"
        );
    }

    #[test]
    fn test_normalize_etag() {
        let check = |test: &str, expect: &str| {
            assert_eq!(normalize_etag(test.as_bytes()), expect.as_bytes())
        };
        check("foo", "foo");
        check("\"foo\"", "foo");
        check("\"nar123\"", "nar123");
        check("", "");
        check("\"\"", "");

        /* Test with -gzip (all combinaison) */
        check("foo-gzip", "foo");
        check("\"foo\"-gzip", "foo");
        check("\"foo-gzip\"", "foo");
    }

    #[test]
    fn test_full_remote_path_to_remote_sync_root_relative() {
        let remote_full_paths_for_root = [
            ("2020", "2020"),
            ("/2021/", "2021"),
            ("/2022/file.docx", "2022/file.docx"),
        ];
        // test against root remote path - result must stay unchanged, leading and trailing slashes must get removed
        for (original, expected) in remote_full_paths_for_root {
            assert_eq!(
                full_remote_path_to_remote_sync_root_relative(original, "/"),
                expected
            );
        }

        let remote_path_non_root = "/Documents/reports";
        let remote_full_paths_for_non_root = [
            (format!("{remote_path_non_root}/2020"), "2020"),
            (format!("{remote_path_non_root}/2021/"), "2021"),
            (
                format!("{remote_path_non_root}/2022/file.docx"),
                "2022/file.docx",
            ),
        ];

        // test against non-root remote path - must always return a proper path as in local db
        for (original, expected) in &remote_full_paths_for_non_root {
            assert_eq!(
                full_remote_path_to_remote_sync_root_relative(original, remote_path_non_root),
                *expected
            );
        }

        // test against non-root remote path with trailing slash - must work the same
        let remote_path_non_root_with_trailing_slash = "/Documents/reports/";
        for (original, expected) in &remote_full_paths_for_non_root {
            assert_eq!(
                full_remote_path_to_remote_sync_root_relative(
                    original,
                    remote_path_non_root_with_trailing_slash
                ),
                *expected
            );
        }

        // test against unrelated remote path - result must stay unchanged
        let remote_path_unrelated = "/Documents1/reports";
        for (original, _) in &remote_full_paths_for_non_root {
            assert_eq!(
                full_remote_path_to_remote_sync_root_relative(original, remote_path_unrelated),
                *original
            );
        }
    }
}
