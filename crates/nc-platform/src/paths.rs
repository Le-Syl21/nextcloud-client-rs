// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! Path helpers. Like upstream's `QString` paths, the port keeps paths as
//! `/`-separated strings (`C:/Users/x/Nextcloud` in `nextcloud.cfg` too) and
//! converts to the native form only where Win32 sees them.

/// `Utility::isPathWindowsDrivePartitionRoot`: `C:`, `C:/` or `C:\`.
/// Upstream answers false outside Windows; the function itself is pure, so
/// the callers test `cfg!(windows)`.
pub fn is_path_windows_drive_partition_root(path: &str) -> bool {
    let b = path.as_bytes();
    // should be 2 or 3 characters length
    if !(2..=3).contains(&path.chars().count()) {
        return false;
    }
    // must match a pattern "[A-Za-z]:"
    if !(b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) {
        return false;
    }
    // final check - last character should be either slash/backslash, or, it should be missing
    b.len() == 2 || b[2] == b'/' || b[2] == b'\\'
}

/// Whether `path` is absolute in the `/`-separated form of the port: a
/// leading `/`, a drive (`C:/...`), or a network share (`//server/share`,
/// `\\server\share`).
pub fn is_absolute_slash_path(path: &str) -> bool {
    let b = path.as_bytes();
    if matches!(b.first(), Some(b'/' | b'\\')) {
        return true;
    }
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\')
}

/// `FileSystem::pathtoUNC` / `longWinPath` on Windows: the verbatim form
/// `\\?\C:\a\b` (backslashes), which Win32 does not normalize (it keeps
/// trailing dots and spaces and lifts the `MAX_PATH` limit). A path that is
/// already a `\\` path is returned with native separators, and a
/// `//server/share` path becomes `\\?\UNC\server\share`.
pub fn long_win_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let native = path.replace('/', "\\");
    if native.starts_with("\\\\?\\") || native.starts_with("\\\\.\\") {
        return native;
    }
    if let Some(unc) = native.strip_prefix("\\\\") {
        return format!("\\\\?\\UNC\\{unc}");
    }
    if native.starts_with('\\') {
        // should not happen as we require the path to be absolute
        return format!("\\\\?{native}");
    }
    format!("\\\\?\\{native}")
}

/// The reverse of [`long_win_path`] for display and comparison: the
/// `/`-separated path without the `\\?\` prefix (`dunce::simplified` for
/// the usual cases).
pub fn from_long_win_path(path: &str) -> String {
    let p = path.replace('\\', "/");
    if let Some(unc) = p.strip_prefix("//?/UNC/") {
        return format!("//{unc}");
    }
    if let Some(rest) = p.strip_prefix("//?/").or_else(|| p.strip_prefix("//./")) {
        return rest.to_owned();
    }
    p
}

/// A path the operating system handed out (`canonicalize`, a directory
/// listing) in the `/`-separated form of the port: on Windows without the
/// `\\?\` prefix and with `/`; unchanged elsewhere.
pub fn from_native(path: &str) -> String {
    if cfg!(windows) {
        from_long_win_path(path)
    } else {
        path.to_owned()
    }
}

/// Whether `path` is absolute on this system: a leading `/` on Unix; on
/// Windows also a drive (`C:/`) or a share (`//server/share`).
pub fn is_absolute(path: &str) -> bool {
    if cfg!(windows) {
        is_absolute_slash_path(path)
    } else {
        path.starts_with('/')
    }
}

/// The `/`-separated form of a native path: on Windows `\` becomes `/`
/// (`QDir::fromNativeSeparators`); elsewhere a backslash is a legal file
/// name character and stays.
pub fn to_slash(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    }
}

/// `QDir::toNativeSeparators`: `\\` on Windows, unchanged elsewhere.
pub fn to_native_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_owned()
    }
}

/// `QDir::cleanPath`: native separators become `/` (Windows), `//` is
/// collapsed, `.` and `..` are resolved, a trailing `/` is dropped (except
/// for a root). On Windows a drive (`C:`) or a share (`//server`) is kept
/// as the root.
pub fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let p = to_slash(p);
    let p = p.as_str();
    // The root part: "" or "/" on Unix; "C:" or "//" on Windows too.
    let (root, rest) = if cfg!(windows) {
        let b = p.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            (&p[..2], &p[2..])
        } else if p.starts_with("//") && !p.starts_with("///") {
            ("/", p)
        } else {
            ("", p)
        }
    } else {
        ("", p)
    };
    let absolute = rest.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("{root}/{joined}")
    } else if joined.is_empty() {
        if root.is_empty() {
            ".".to_owned()
        } else {
            root.to_owned()
        }
    } else {
        format!("{root}{joined}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // testutility.cpp: testIsPathWindowsDrivePartitionRoot
    #[test]
    fn drive_partition_root() {
        assert!(is_path_windows_drive_partition_root("C:"));
        assert!(is_path_windows_drive_partition_root("c:/"));
        assert!(is_path_windows_drive_partition_root("D:\\"));
        assert!(!is_path_windows_drive_partition_root("C"));
        assert!(!is_path_windows_drive_partition_root("1:/"));
        assert!(!is_path_windows_drive_partition_root("C:/a"));
        assert!(!is_path_windows_drive_partition_root("C:a"));
        assert!(!is_path_windows_drive_partition_root(""));
    }

    #[test]
    fn absolute() {
        assert!(is_absolute_slash_path("/home/x"));
        assert!(is_absolute_slash_path("C:/Users/x"));
        assert!(is_absolute_slash_path("c:\\Users"));
        assert!(is_absolute_slash_path("//server/share/x"));
        assert!(!is_absolute_slash_path("C:"));
        assert!(!is_absolute_slash_path("C:x"));
        assert!(!is_absolute_slash_path("rel/path"));
        assert!(!is_absolute_slash_path(""));
    }

    #[test]
    fn clean() {
        assert_eq!(clean_path("/a//b/./c/../d/"), "/a/b/d");
        assert_eq!(clean_path("a/.."), ".");
        assert_eq!(clean_path("../a"), "../a");
        assert_eq!(clean_path(""), "");
        assert_eq!(clean_path("/"), "/");
        if cfg!(windows) {
            assert_eq!(clean_path("C:\\Users\\x\\..\\y\\"), "C:/Users/y");
            assert_eq!(clean_path("C:/"), "C:/");
            assert_eq!(clean_path("//srv/share/./a"), "//srv/share/a");
        }
    }

    // testlongpath.cpp: check_long_win_path
    #[test]
    fn long_win() {
        assert_eq!(long_win_path("C:/a/b."), "\\\\?\\C:\\a\\b.");
        assert_eq!(long_win_path("\\\\?\\C:\\a"), "\\\\?\\C:\\a");
        assert_eq!(long_win_path("//srv/share/f"), "\\\\?\\UNC\\srv\\share\\f");
        assert_eq!(long_win_path(""), "");
        let long = format!("C:/{}", "a/".repeat(200));
        assert!(long_win_path(&long).starts_with("\\\\?\\C:\\a\\a"));
        assert_eq!(from_long_win_path("\\\\?\\C:\\a\\b"), "C:/a/b");
        assert_eq!(from_long_win_path("\\\\?\\UNC\\srv\\share"), "//srv/share");
        assert_eq!(from_long_win_path("C:\\a"), "C:/a");
    }
}
