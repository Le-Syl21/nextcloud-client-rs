// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Port of parts of upstream `src/common/utility.{h,cpp}` (nextcloud/desktop v34.0.5).

//! Small helpers from upstream `OCC::Utility` needed by the journal and the engine.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// `Utility::isWindows()`.
pub const fn is_windows() -> bool {
    cfg!(windows)
}

/// `Utility::isMac()`.
pub const fn is_mac() -> bool {
    cfg!(target_os = "macos")
}

/// `Utility::isUnix()`.
pub const fn is_unix() -> bool {
    cfg!(all(unix, not(target_os = "macos")))
}

/// `Utility::isLinux()` (use with care).
pub const fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// `Utility::isBSD()` (use with care, does not match macOS).
pub const fn is_bsd() -> bool {
    cfg!(any(
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
}

// 0 = not overridden, 1 = false, 2 = true.
static FS_CASE_PRESERVING_OVERRIDE: AtomicU8 = AtomicU8::new(0);

fn fs_case_preserving_default() -> bool {
    static DEFAULT: OnceLock<bool> = OnceLock::new();
    *DEFAULT.get_or_init(|| {
        // Upstream reads OWNCLOUD_TEST_CASE_PRESERVING once, at static init.
        match std::env::var("OWNCLOUD_TEST_CASE_PRESERVING") {
            Ok(env) if !env.is_empty() => qbytearray_to_int(env.as_bytes()) != 0,
            _ => is_windows() || is_mac(),
        }
    })
}

/// `QByteArray::toInt()`: base 10, surrounding whitespace allowed, 0 on failure.
fn qbytearray_to_int(value: &[u8]) -> i32 {
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// `Utility::fsCasePreserving()`: whether the local file system is case
/// preserving but case insensitive (Windows, macOS), overridable with the
/// `OWNCLOUD_TEST_CASE_PRESERVING` environment variable.
pub fn fs_case_preserving() -> bool {
    match FS_CASE_PRESERVING_OVERRIDE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => fs_case_preserving_default(),
    }
}

/// Test hook equivalent to upstream's exported `fsCasePreserving_override`
/// variable. `None` restores the default.
pub fn set_fs_case_preserving_override(value: Option<bool>) {
    let v = match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    FS_CASE_PRESERVING_OVERRIDE.store(v, Ordering::Relaxed);
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn contains(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

fn basename(name: &str) -> &str {
    match name.rfind('/') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// `Utility::isConflictFile()`.
pub fn is_conflict_file(name: &str) -> bool {
    let bname = basename(name);
    contains(bname, "_conflict-")
        || contains(bname, "(conflicted copy")
        || is_case_clash_conflict_file(name)
}

/// `Utility::isCaseClashConflictFile()`.
pub fn is_case_clash_conflict_file(name: &str) -> bool {
    contains(basename(name), "(case clash from")
}

/// `Utility::conflictFileBaseNameFromPattern()`: find the base name for a
/// conflict file name using the name pattern only. Returns an empty value if
/// it's not a conflict file by pattern.
///
/// This must be able to deal with conflict files for conflict files: only the
/// outermost (rightmost) conflict marker is stripped.
pub fn conflict_file_base_name_from_pattern(conflict_name: &[u8]) -> Vec<u8> {
    let start_old = rfind(conflict_name, b"_conflict-");

    // A single space before "(conflicted copy" is considered part of the tag
    let mut start_new = rfind(conflict_name, b"(conflicted copy");
    if let Some(s) = start_new
        && s > 0
        && conflict_name[s - 1] == b' '
    {
        start_new = Some(s - 1);
    }

    // The rightmost tag is relevant
    let tag_start = match (start_old, start_new) {
        (None, None) => return Vec::new(),
        (a, b) => a.max(b).expect("one is set"),
    };

    // Find the end of the tag
    let mut tag_end = conflict_name.len();
    if let Some(dot) = rfind(conflict_name, b".") {
        // dot could be part of user name for new tag!
        if dot > tag_start {
            tag_end = dot;
        }
    }
    if Some(tag_start) == start_new
        && let Some(paren) = conflict_name[tag_start..].iter().position(|&c| c == b')')
    {
        tag_end = tag_start + paren + 1;
    }
    let mut result = conflict_name[..tag_start].to_vec();
    result.extend_from_slice(&conflict_name[tag_end..]);
    result
}

/// `Utility::leadingSlashPath()`.
pub fn leading_slash_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// `Utility::trailingSlashPath()`.
pub fn trailing_slash_path(path: &str) -> String {
    if path.ends_with('/') {
        path.to_owned()
    } else {
        format!("{path}/")
    }
}

/// `Utility::noLeadingSlashPath()`.
pub fn no_leading_slash_path(path: &str) -> &str {
    if path.len() > 1 && path.starts_with('/') {
        &path[1..]
    } else {
        path
    }
}

/// `Utility::noTrailingSlashPath()`.
pub fn no_trailing_slash_path(path: &str) -> &str {
    if path.len() > 1 && path.ends_with('/') {
        &path[..path.len() - 1]
    } else {
        path
    }
}
