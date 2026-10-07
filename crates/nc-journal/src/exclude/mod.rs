/*
 * libcsync -- a library to sync a directory with another
 *
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2012 ownCloud GmbH
 * SPDX-FileCopyrightText: 2008-2013 Andreas Schneider <asn@cryptomilk.org>
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

//! The csync exclude engine (`ExcludedFiles`), ported from upstream
//! `src/csync/csync_exclude.{h,cpp}`.
//!
//! Exclude patterns are loaded from files (see
//! [`ExcludedFiles::add_exclude_file_path`] and
//! [`ExcludedFiles::reload_exclude_files`]) or added manually. They are
//! compiled into a handful of combined regular expressions per base path,
//! exactly like upstream does with `QRegularExpression`.
//!
//! # Regex dialect
//!
//! Upstream builds PCRE2 patterns (through `QRegularExpression`). The
//! textual patterns are reproduced byte for byte (they can be inspected with
//! [`ExcludedFiles::convert_to_regexp_syntax`] and the `pattern()` accessors
//! used by the tests), then translated to the `regex` crate dialect by
//! [`pcre_to_rust_regex`] right before compilation. The translation keeps
//! PCRE semantics for everything the generator can produce:
//!
//! * `\<` / `\>` are literal characters in PCRE but word boundaries in Rust;
//! * PCRE accepts a backslash before any non-alphanumeric character
//!   (`QRegularExpression::escape()` produces `\é`, `\💩`, `\` + CR...), Rust
//!   only before ASCII punctuation;
//! * `\0` (the escape of NUL) is `\x00`;
//! * `[` inside a bracket expression is literal in PCRE, a nested class in
//!   Rust; `&&`, `--` and `~~` are set operators in Rust classes;
//! * without the multiline option, PCRE's `$` also matches right before a
//!   final newline; it becomes `(?:\n?\z)`.
//!
//! An invalid pattern never matches (like an invalid `QRegularExpression`).

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use regex::Regex;

use crate::csync::ItemType;

/// Result of an exclude check (`CSYNC_EXCLUDE_TYPE` in upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ExcludeType {
    /// `CSYNC_NOT_EXCLUDED`
    NotExcluded = 0,
    /// `CSYNC_FILE_SILENTLY_EXCLUDED`
    FileSilentlyExcluded,
    /// `CSYNC_FILE_EXCLUDE_AND_REMOVE`
    FileExcludeAndRemove,
    /// `CSYNC_FILE_EXCLUDE_LIST`
    FileExcludeList,
    /// `CSYNC_FILE_EXCLUDE_INVALID_CHAR`
    FileExcludeInvalidChar,
    /// `CSYNC_FILE_EXCLUDE_TRAILING_SPACE`
    FileExcludeTrailingSpace,
    /// `CSYNC_FILE_EXCLUDE_LONG_FILENAME`
    FileExcludeLongFilename,
    /// `CSYNC_FILE_EXCLUDE_HIDDEN`
    FileExcludeHidden,
    /// `CSYNC_FILE_EXCLUDE_STAT_FAILED`
    FileExcludeStatFailed,
    /// `CSYNC_FILE_EXCLUDE_CONFLICT`
    FileExcludeConflict,
    /// `CSYNC_FILE_EXCLUDE_CASE_CLASH_CONFLICT`
    FileExcludeCaseClashConflict,
    /// `CSYNC_FILE_EXCLUDE_CANNOT_ENCODE`
    FileExcludeCannotEncode,
    /// `CSYNC_FILE_EXCLUDE_SERVER_BLACKLISTED`
    FileExcludeServerBlacklisted,
    /// `CSYNC_FILE_EXCLUDE_LEADING_SPACE`
    FileExcludeLeadingSpace,
    /// `CSYNC_FILE_EXCLUDE_LEADING_AND_TRAILING_SPACE`
    FileExcludeLeadingAndTrailingSpace,
    /// `CSYNC_FILE_LOCKED_SILENTLY_EXCLUDED`
    FileLockedSilentlyExcluded,
}

/// Client version used to evaluate `#!version` directives
/// (`MIRALL_VERSION_MAJOR/MINOR/PATCH` of the reference upstream, v34.0.5).
pub const CLIENT_VERSION: Version = crate::UPSTREAM_VERSION;

/// `ExcludedFiles::Version`
pub type Version = (i32, i32, i32);

/// The upstream default exclude list (`sync-exclude.lst` of v34.0.5).
pub const DEFAULT_SYNC_EXCLUDE_LST: &str = include_str!("../../data/sync-exclude.lst");

use crate::utility::{fs_case_preserving, is_case_clash_conflict_file, is_conflict_file};

/// `QByteArray::toInt()` (base 10): 0 when the text is not a number.
fn qt_to_int(bytes: &[u8]) -> i32 {
    std::str::from_utf8(qt_byte_trimmed(bytes))
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0)
}

/// `QByteArray::trimmed()`: strips ASCII whitespace (`\t \n \v \f \r` and space).
fn qt_byte_trimmed(bytes: &[u8]) -> &[u8] {
    let is_space = |b: &u8| matches!(*b, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ');
    let start = bytes
        .iter()
        .position(|b| !is_space(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_space(b))
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// Number of UTF-16 code units, i.e. `QString::size()`.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ---------------------------------------------------------------------------
// Free functions of csync_exclude.cpp
// ---------------------------------------------------------------------------

/// `csync_exclude_expand_escapes()`: expands C-like escape sequences.
///
/// `\*`, `\?`, `\[` and `\\` (and unknown escapes) are kept as they are and
/// processed during regex translation. Like upstream, a trailing lone
/// backslash is followed by the terminating NUL byte of the buffer, which
/// ends up in the result (`"\\"` becomes `"\\\0"`).
pub fn csync_exclude_expand_escapes(input: &mut Vec<u8>) {
    let len = input.len();
    let mut out = Vec::with_capacity(len + 1);
    let at = |i: usize| input.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while i < len {
        if input[i] == b'\\' {
            match at(i + 1) {
                b'\'' => out.push(b'\''),
                b'"' => out.push(b'"'),
                b'?' => out.push(b'?'),
                b'#' => out.push(b'#'),
                b'a' => out.push(0x07),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0C),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'v' => out.push(0x0B),
                next => {
                    // '\*' '\?' '\[' '\\' will be processed during regex translation
                    // '\\' is intentionally not expanded here (to avoid '\\*' and '\*'
                    // ending up meaning the same thing)
                    out.push(input[i]);
                    out.push(next);
                }
            }
            i += 2;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    *input = out;
}

// See http://support.microsoft.com/kb/74496 and
// https://msdn.microsoft.com/en-us/library/windows/desktop/aa365247(v=vs.85).aspx
// Additionally, we ignore '$Recycle.Bin', see https://github.com/owncloud/client/issues/2955
const WIN_RESERVED_WORDS_3: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
const WIN_RESERVED_WORDS_4: [&str; 18] = [
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3",
    "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
const WIN_RESERVED_WORDS_N: [&str; 2] = ["CLOCK$", "$Recycle.Bin"];

/// `csync_is_windows_reserved_word()`: checks whether a file name is
/// reserved by Windows.
pub fn csync_is_windows_reserved_word(filename: &str) -> bool {
    let units: Vec<u16> = filename.encode_utf16().collect();
    let len = units.len();
    let unit_is = |i: usize, c: char| units.get(i) == Some(&(c as u16));
    let eq_ci = |units: &[u16], word: &str| {
        let lower = |u: u16| {
            if (u16::from(b'A')..=u16::from(b'Z')).contains(&u) {
                u + 32
            } else {
                u
            }
        };
        units.len() == word.len()
            && units
                .iter()
                .zip(word.bytes())
                .all(|(&a, b)| lower(a) == lower(u16::from(b)))
    };

    // Drive letters
    if len == 2 && unit_is(1, ':') {
        let c = units[0];
        if (u16::from(b'a')..=u16::from(b'z')).contains(&c) {
            return true;
        }
        if (u16::from(b'A')..=u16::from(b'Z')).contains(&c) {
            return true;
        }
    }

    if (len == 3 || (len > 3 && unit_is(3, '.')))
        && WIN_RESERVED_WORDS_3
            .iter()
            .any(|word| eq_ci(&units[..3], word))
    {
        return true;
    }

    if (len == 4 || (len > 4 && unit_is(4, '.')))
        && WIN_RESERVED_WORDS_4
            .iter()
            .any(|word| eq_ci(&units[..4], word))
    {
        return true;
    }

    WIN_RESERVED_WORDS_N.iter().any(|word| eq_ci(&units, word))
}

/// `_csync_excluded_common()`: the built-in, pattern independent excludes.
fn csync_excluded_common(path: &str, exclude_conflict_files: bool) -> ExcludeType {
    /* split up the path */
    let bname = &path[path.rfind('/').map_or(0, |i| i + 1)..];

    let blen = utf16_len(bname);
    let starts_with_ci = |prefix: &str| {
        bname.len() >= prefix.len()
            && bname.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    };
    // 9 = strlen(".sync_.db")
    if blen >= 9 && bname.starts_with('.') {
        if bname.contains(".db")
            && (starts_with_ci("._sync_") // "._sync_*.db*"
                || starts_with_ci(".sync_") // ".sync_*.db*"
                || starts_with_ci(".csync_journal.db"))
        // ".csync_journal.db*"
        {
            return ExcludeType::FileSilentlyExcluded;
        }
        if starts_with_ci(".owncloudsync.log") {
            // ".owncloudsync.log*"
            return ExcludeType::FileSilentlyExcluded;
        }
        if starts_with_ci(".nextcloudsync.log") {
            // ".nextcloudsync.log*"
            return ExcludeType::FileSilentlyExcluded;
        }
        if starts_with_ci(".nextcloudpermissions.log") {
            // ".nextcloudpermissions.log*"
            return ExcludeType::FileSilentlyExcluded;
        }
    }

    // check the strlen and ignore the file if its name is longer than 254 chars.
    // whenever changing this also check createDownloadTmpFileName
    if blen > 254 {
        return ExcludeType::FileExcludeLongFilename;
    }

    #[cfg(windows)]
    {
        // Windows cannot sync files ending in spaces (#2176). It also cannot
        // distinguish files ending in '.' from files without an ending,
        // as '.' is a separator that is not stored internally, so let's
        // not allow to sync those to avoid file loss/ambiguities (#416)
        if blen > 1 && bname.ends_with('.') {
            return ExcludeType::FileExcludeInvalidChar;
        }

        if csync_is_windows_reserved_word(bname) {
            return ExcludeType::FileSilentlyExcluded;
        }

        // Filter out characters not allowed in a filename on windows
        for c in path.encode_utf16() {
            if c < 32 {
                return ExcludeType::FileExcludeInvalidChar;
            }
            if let Ok(b) = u8::try_from(c)
                && matches!(b, b'\\' | b':' | b'?' | b'*' | b'"' | b'>' | b'<' | b'|')
            {
                return ExcludeType::FileExcludeInvalidChar;
            }
        }
    }

    /* Do not sync desktop.ini files anywhere in the tree. */
    if bname.eq_ignore_ascii_case("desktop.ini") {
        return ExcludeType::FileSilentlyExcluded;
    }

    if exclude_conflict_files {
        if is_case_clash_conflict_file(path) {
            return ExcludeType::FileExcludeCaseClashConflict;
        } else if is_conflict_file(path) {
            return ExcludeType::FileExcludeConflict;
        }
    }

    ExcludeType::NotExcluded
}

/// `leftIncludeLast()`: left part of `arr` up to and including the last `c`,
/// ignoring the very last character (`arr.left(arr.lastIndexOf(c, arr.size() - 2) + 1)`).
fn left_include_last(arr: &str, c: char) -> String {
    // QString::lastIndexOf(c, from) with from = size - 2. A negative `from`
    // counts from the end: -1 searches the whole string, -2 (empty string)
    // finds nothing.
    let size = arr.len();
    let haystack = match size {
        0 => "",
        1 => arr,
        _ => {
            // Exclude the last character. `c` is ASCII, so cutting inside a
            // multi-byte character cannot create or hide a match.
            let mut end = size - 1;
            while !arr.is_char_boundary(end) {
                end -= 1;
            }
            &arr[..end]
        }
    };
    match haystack.rfind(c) {
        Some(idx) => arr[..idx + c.len_utf8()].to_string(),
        None => String::new(),
    }
}

/// `QRegularExpression::escape()`: escapes every character except
/// `[A-Za-z0-9_]` with a backslash; NUL becomes `\0`.
fn qregex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c == '\0' {
            out.push_str("\\0");
        } else if !(c.is_ascii_alphanumeric() || c == '_') {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Translates a PCRE pattern as generated by the exclude engine to the
/// `regex` crate dialect, keeping PCRE semantics (see the module docs).
pub fn pcre_to_rust_regex(pattern: &str) -> String {
    fn push_escape(out: &mut String, d: char) {
        if d.is_ascii_alphanumeric() {
            if d == '0' {
                out.push_str("\\x00");
            } else {
                // PCRE escape classes like \d or \w (only reachable from user
                // written bracket expressions).
                out.push('\\');
                out.push(d);
            }
        } else if d == '<' || d == '>' {
            out.push(d);
        } else if d.is_ascii_punctuation() {
            out.push('\\');
            out.push(d);
        } else {
            out.push_str(&regex::escape(d.encode_utf8(&mut [0; 4])));
        }
    }

    let chars: Vec<char> = pattern.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut i = 0;
    while i < n {
        match chars[i] {
            '\\' => {
                if i + 1 < n {
                    push_escape(&mut out, chars[i + 1]);
                    i += 2;
                } else {
                    // "\ at end of pattern": invalid in PCRE too.
                    out.push('\\');
                    i += 1;
                }
            }
            '$' => {
                out.push_str("(?:\\n?\\z)");
                i += 1;
            }
            '[' => {
                out.push('[');
                i += 1;
                if i < n && chars[i] == '^' {
                    out.push('^');
                    i += 1;
                }
                if i < n && chars[i] == ']' {
                    out.push_str("\\]");
                    i += 1;
                }
                while i < n && chars[i] != ']' {
                    let c = chars[i];
                    match c {
                        '\\' if i + 1 < n => {
                            push_escape(&mut out, chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        '[' => {
                            // POSIX class "[:name:]" is common to both dialects.
                            if i + 1 < n && chars[i + 1] == ':' {
                                let rest: String = chars[i..].iter().collect();
                                if let Some(end) = rest.find(":]") {
                                    out.push_str(&rest[..end + 2]);
                                    i += rest[..end + 2].chars().count();
                                    continue;
                                }
                            }
                            out.push_str("\\[");
                        }
                        '&' | '~' => {
                            out.push('\\');
                            out.push(c);
                        }
                        '-' if i > 0 && chars[i - 1] == '-' => out.push_str("\\-"),
                        _ => out.push(c),
                    }
                    i += 1;
                }
                if i < n {
                    out.push(']');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// A compiled pattern that remembers its PCRE source (`QRegularExpression`).
#[derive(Debug, Clone)]
pub struct PatternRegex {
    pattern: String,
    regex: Option<Regex>,
}

/// Which named group of an exclude regex matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    None,
    Exclude,
    ExcludeRemove,
    Other,
}

impl PatternRegex {
    fn new(pattern: String, case_insensitive: bool) -> Self {
        let mut translated = pcre_to_rust_regex(&pattern);
        if case_insensitive {
            translated.insert_str(0, "(?i)");
        }
        let regex = match Regex::new(&translated) {
            Ok(regex) => Some(regex),
            Err(err) => {
                log::warn!("invalid exclude regular expression {pattern:?}: {err}");
                None
            }
        };
        Self { pattern, regex }
    }

    /// The PCRE pattern, identical to upstream's `QRegularExpression::pattern()`.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    fn capture(&self, subject: &str) -> Capture {
        let Some(regex) = &self.regex else {
            return Capture::None;
        };
        match regex.captures(subject) {
            None => Capture::None,
            Some(caps) if caps.name("exclude").is_some() => Capture::Exclude,
            Some(caps) if caps.name("excluderemove").is_some() => Capture::ExcludeRemove,
            Some(_) => Capture::Other,
        }
    }
}

/// Manages file/directory exclusion (`ExcludedFiles`).
///
/// Most commonly exclude patterns are loaded from files. See
/// [`add_exclude_file_path`](Self::add_exclude_file_path) and
/// [`reload_exclude_files`](Self::reload_exclude_files).
///
/// Excluded files are primarily relevant for sync runs, and for file watcher
/// filtering.
///
/// Excluded files and ignored files are the same thing. But the selective
/// sync blacklist functionality is a different thing entirely.
#[derive(Debug, Clone)]
pub struct ExcludedFiles {
    local_path: String,

    /// Files to load excludes from
    exclude_files: BTreeMap<String, Vec<String>>,

    /// Exclude patterns added with add_manual_exclude()
    manual_excludes: BTreeMap<String, Vec<String>>,

    /// List of all active exclude patterns
    all_excludes: BTreeMap<String, Vec<String>>,

    /// see prepare()
    bname_traversal_regex_file: BTreeMap<String, PatternRegex>,
    bname_traversal_regex_dir: BTreeMap<String, PatternRegex>,
    full_traversal_regex_file: BTreeMap<String, PatternRegex>,
    full_traversal_regex_dir: BTreeMap<String, PatternRegex>,
    full_regex_file: BTreeMap<String, PatternRegex>,
    full_regex_dir: BTreeMap<String, PatternRegex>,

    exclude_conflict_files: bool,

    /// Whether * and ? in patterns can match a /
    ///
    /// Unfortunately this was how matching was done on Windows so
    /// it continues to be enabled there.
    wildcards_match_slash: bool,

    /// The client version. Used to evaluate version-dependent excludes,
    /// see version_directive_keep_next_line().
    client_version: Version,
}

impl Default for ExcludedFiles {
    fn default() -> Self {
        Self::new("/")
    }
}

impl ExcludedFiles {
    /// Creates an exclude engine for the sync folder `local_path`, which must
    /// end with a `/` (upstream default: `"/"`).
    pub fn new(local_path: &str) -> Self {
        debug_assert!(local_path.ends_with('/'));
        Self {
            local_path: local_path.to_string(),
            exclude_files: BTreeMap::new(),
            manual_excludes: BTreeMap::new(),
            all_excludes: BTreeMap::new(),
            bname_traversal_regex_file: BTreeMap::new(),
            bname_traversal_regex_dir: BTreeMap::new(),
            full_traversal_regex_file: BTreeMap::new(),
            full_traversal_regex_dir: BTreeMap::new(),
            full_regex_file: BTreeMap::new(),
            full_regex_dir: BTreeMap::new(),
            exclude_conflict_files: true,
            // Windows used to use PathMatchSpec which allows *foo to match abc/deffoo.
            wildcards_match_slash: cfg!(windows),
            client_version: CLIENT_VERSION,
        }
    }

    /// Adds a new path to a file containing exclude patterns.
    ///
    /// Does not load the file. Use [`reload_exclude_files`](Self::reload_exclude_files) afterwards.
    pub fn add_exclude_file_path(&mut self, path: &str) {
        let file_name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
        let base_path = if file_name.eq_ignore_ascii_case("sync-exclude.lst") {
            self.local_path.clone()
        } else {
            left_include_last(path, '/')
        };
        let list = self.exclude_files.entry(base_path).or_default();
        if !list.iter().any(|p| p == path) {
            list.push(path.to_string());
        }
    }

    /// Whether conflict files shall be excluded. Defaults to true.
    pub fn set_exclude_conflict_files(&mut self, onoff: bool) {
        self.exclude_conflict_files = onoff;
    }

    /// Checks whether a file or directory should be excluded.
    ///
    /// * `file_path`: the absolute path to the file
    /// * `base_path`: folder path from which to apply exclude rules, ends with a /
    pub fn is_excluded(&self, file_path: &str, base_path: &str, exclude_hidden: bool) -> bool {
        let Some(rest) = strip_prefix_fs(file_path, base_path) else {
            // Mark paths we're not responsible for as excluded...
            return true;
        };

        //TODO this seems a waste, hidden files are ignored before hitting this function it seems
        if exclude_hidden {
            let mut path = file_path.to_string();
            let base_len = utf16_len(base_path);
            // Check all path subcomponents, but to *not* check the base path:
            // We do want to be able to sync with a hidden folder as the target.
            while utf16_len(&path) > base_len {
                let file_name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
                // FileSystem::isFileHidden() is QFileInfo::isHidden(), which on
                // Unix means "starts with a dot".
                if file_name != ".sync-exclude.lst" && file_name.starts_with('.') {
                    return true;
                }

                // Get the parent path
                path = qfileinfo_absolute_path(&path);
            }
        }

        let ty = if Path::new(file_path).is_dir() {
            ItemType::Directory
        } else {
            ItemType::File
        };

        let relative_path = rest.strip_suffix('/').unwrap_or(rest);

        self.full_pattern_match(relative_path, ty) != ExcludeType::NotExcluded
    }

    /// Adds an exclude pattern anchored to the sync folder.
    ///
    /// Primarily used in tests. Patterns added this way are preserved when
    /// [`reload_exclude_files`](Self::reload_exclude_files) is called.
    pub fn add_manual_exclude(&mut self, expr: &str) {
        let local_path = self.local_path.clone();
        self.add_manual_exclude_with_base(expr, &local_path);
    }

    /// Adds an exclude pattern anchored to `base_path` (which ends with a `/`).
    pub fn add_manual_exclude_with_base(&mut self, expr: &str, base_path: &str) {
        debug_assert!(base_path.ends_with('/'));

        if expr.trim() == "*" {
            return;
        }

        let key = base_path.to_string();
        self.manual_excludes
            .entry(key.clone())
            .or_default()
            .push(expr.to_string());
        self.all_excludes
            .entry(key.clone())
            .or_default()
            .push(expr.to_string());
        self.prepare_base(&key);
    }

    /// Removes all manually added exclude patterns. Primarily used in tests.
    pub fn clear_manual_excludes(&mut self) {
        self.manual_excludes.clear();
        self.reload_exclude_files();
    }

    /// Adjusts behavior of wildcards. Only used for testing.
    pub fn set_wildcards_match_slash(&mut self, onoff: bool) {
        self.wildcards_match_slash = onoff;
        self.prepare_all();
    }

    /// Sets the client version, only used for testing.
    pub fn set_client_version(&mut self, version: Version) {
        self.client_version = version;
    }

    /// Checks whether the given path should be excluded in a traversal situation.
    ///
    /// It does only part of the work that the full match does because it's
    /// assumed that all leading directories have been run through
    /// traversal before. This can be significantly faster.
    ///
    /// That means for `foo/bar/file` only (`foo/bar/file`, `file`) is checked
    /// against the exclude patterns.
    ///
    /// `path` is folder-relative, should not start with a /.
    ///
    /// Note that this only matches patterns. It does not check whether the file
    /// or directory pointed to is hidden (or whether it even exists).
    ///
    /// When `filetype` is a directory containing a readable `.sync-exclude.lst`,
    /// that file is registered and all exclude files are reloaded.
    pub fn traversal_pattern_match(&mut self, path: &str, filetype: ItemType) -> ExcludeType {
        let m = csync_excluded_common(path, self.exclude_conflict_files);
        if m != ExcludeType::NotExcluded {
            return m;
        }
        if self.all_excludes.is_empty() {
            return ExcludeType::NotExcluded;
        }

        // Directories are guaranteed to be visited before their files
        if filetype == ItemType::Directory {
            let base_path = format!("{}{}/", self.local_path, path);
            let absolute_path = format!("{base_path}.sync-exclude.lst");
            if is_readable(&absolute_path) {
                self.add_exclude_file_path(&absolute_path);
                self.reload_exclude_files();
            } else {
                log::trace!("System exclude list file could not be read: {absolute_path}");
            }
        }

        // Check the bname part of the path to see whether the full
        // regex should be run.
        let bname_str = &path[path.rfind('/').map_or(0, |i| i + 1)..];

        let local_len = self.local_path.len();
        let mut base_path = format!("{}{}", self.local_path, path);
        while base_path.len() > local_len {
            base_path = left_include_last(&base_path, '/');
            let regex = match filetype {
                ItemType::Directory => self.bname_traversal_regex_dir.get(&base_path),
                ItemType::File => self.bname_traversal_regex_file.get(&base_path),
                _ => None,
            };
            let Some(regex) = regex else {
                continue;
            };
            match regex.capture(bname_str) {
                Capture::None => return ExcludeType::NotExcluded,
                Capture::Exclude => return ExcludeType::FileExcludeList,
                Capture::ExcludeRemove => return ExcludeType::FileExcludeAndRemove,
                Capture::Other => {}
            }
        }

        // third capture: full path matching is triggered
        base_path = format!("{}{}", self.local_path, path);
        while base_path.len() > local_len {
            base_path = left_include_last(&base_path, '/');
            let regex = match filetype {
                ItemType::Directory => self.full_traversal_regex_dir.get(&base_path),
                ItemType::File => self.full_traversal_regex_file.get(&base_path),
                _ => None,
            };
            let Some(regex) = regex else {
                continue;
            };
            match regex.capture(path) {
                Capture::Exclude => return ExcludeType::FileExcludeList,
                Capture::ExcludeRemove => return ExcludeType::FileExcludeAndRemove,
                Capture::None | Capture::Other => {}
            }
        }
        ExcludeType::NotExcluded
    }

    /// Provides all active exclude patterns (deduplicated; sorted, whereas
    /// upstream returns them in `QSet` order).
    pub fn active_exclude_patterns(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self.all_excludes.values().flatten().collect();
        set.into_iter().cloned().collect()
    }

    /// Reloads the exclude patterns from the registered paths.
    ///
    /// Returns false if an existing exclude file could not be opened.
    /// Registered files that do not exist are dropped from the list.
    pub fn reload_exclude_files(&mut self) -> bool {
        self.all_excludes.clear();
        // clear all regex
        self.clear_regexes();

        let mut success = true;
        let keys: Vec<String> = self.exclude_files.keys().cloned().collect();
        for base_path in keys {
            let mut files = self.exclude_files.remove(&base_path).unwrap_or_default();
            let mut kept = Vec::with_capacity(files.len());
            for exclude_file in files.drain(..) {
                let path = Path::new(&exclude_file);
                if !path.exists() {
                    log::warn!("Exclude list file does not exist, skipping: {exclude_file}");
                    continue;
                }

                match open_and_read(path) {
                    Some(Ok(content)) => self.load_exclude_file_patterns(&base_path, &content),
                    Some(Err(err)) => {
                        // e.g. "Access to the cloud file is denied." when the exclude
                        // list is dehydrated on a VFS-enabled sync folder
                        log::warn!(
                            "failed to load exclude patterns from file, assume empty ignore list fileName={exclude_file} error={err}"
                        );
                        self.load_exclude_file_patterns(&base_path, b"");
                    }
                    None => {
                        success = false;
                        log::warn!("System exclude list file could not be opened: {exclude_file}");
                    }
                }
                kept.push(exclude_file);
            }
            self.exclude_files.insert(base_path, kept);
        }

        let manual: Vec<(String, Vec<String>)> = self
            .manual_excludes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, values) in manual {
            self.all_excludes
                .entry(key.clone())
                .or_default()
                .extend(values);
            self.prepare_base(&key);
        }

        success
    }

    /// Loads the exclude patterns of an exclude file (`content`) for `base_path`.
    pub fn load_exclude_file_patterns(&mut self, base_path: &str, content: &[u8]) {
        let mut patterns = Vec::new();
        let mut lines = QtLines::new(content);
        while let Some(raw) = lines.next() {
            let mut line = qt_byte_trimmed(raw).to_vec();

            if line.starts_with(b"#!version") && !self.version_directive_keep_next_line(&line) {
                lines.next();
            }
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            if String::from_utf8_lossy(&line).trim() == "*" {
                continue;
            }
            csync_exclude_expand_escapes(&mut line);
            patterns.push(String::from_utf8_lossy(&line).into_owned());
        }
        let entry = self.all_excludes.entry(base_path.to_string()).or_default();
        entry.extend(patterns);

        // nothing to prepare if the user decided to not exclude anything
        if !entry.is_empty() {
            self.prepare_base(base_path);
        }
    }

    /// Returns true if the version directive indicates the next line
    /// should be kept (false: skip it).
    ///
    /// A version directive has the form `#!version <op> <version>`
    /// where `<op>` can be `<`, `<=`, `==`, `>`, `>=` and `<version>` can be
    /// any version like 2.5.0.
    ///
    /// Example:
    ///
    /// ```text
    /// #!version < 2.5.0
    /// myexclude
    /// ```
    ///
    /// Would enable the "myexclude" pattern only for versions before 2.5.0.
    pub fn version_directive_keep_next_line(&self, directive: &[u8]) -> bool {
        if !directive.starts_with(b"#!version") {
            return true;
        }
        let args: Vec<&[u8]> = directive.split(|&b| b == b' ').collect();
        if args.len() != 3 {
            return true;
        }
        let op = args[1];
        let arg_versions: Vec<&[u8]> = args[2].split(|&b| b == b'.').collect();
        if arg_versions.len() != 3 {
            return true;
        }

        let arg_version = (
            qt_to_int(arg_versions[0]),
            qt_to_int(arg_versions[1]),
            qt_to_int(arg_versions[2]),
        );
        let client = self.client_version;
        match op {
            b"<=" => client <= arg_version,
            b"<" => client < arg_version,
            b">" => client > arg_version,
            b">=" => client >= arg_version,
            b"==" => client == arg_version,
            _ => true,
        }
    }

    /// Matches the exclude patterns against the full path.
    ///
    /// `p` is folder-relative, should not start with a /.
    ///
    /// Note that this only matches patterns. It does not check whether the file
    /// or directory pointed to is hidden (or whether it even exists).
    pub fn full_pattern_match(&self, p: &str, filetype: ItemType) -> ExcludeType {
        let m = csync_excluded_common(p, self.exclude_conflict_files);
        if m != ExcludeType::NotExcluded {
            return m;
        }
        if self.all_excludes.is_empty() {
            return ExcludeType::NotExcluded;
        }

        // `path` seems to always be relative to `_localPath`, the tests however have not been
        // written that way... this makes the tests happy for now. TODO Fix the tests at some point
        let path = p.strip_prefix(self.local_path.as_str()).unwrap_or(p);

        let local_len = self.local_path.len();
        let mut base_path = format!("{}{}", self.local_path, path);
        while base_path.len() > local_len {
            base_path = left_include_last(&base_path, '/');
            let regex = match filetype {
                ItemType::Directory => self.full_regex_dir.get(&base_path),
                ItemType::File => self.full_regex_file.get(&base_path),
                _ => None,
            };
            let Some(regex) = regex else {
                continue;
            };
            match regex.capture(p) {
                Capture::Exclude => return ExcludeType::FileExcludeList,
                Capture::ExcludeRemove => return ExcludeType::FileExcludeAndRemove,
                Capture::None | Capture::Other => {}
            }
        }

        ExcludeType::NotExcluded
    }

    /// Translates an exclude pattern (`*`, `?`, `[...]`, escapes) to a PCRE
    /// regular expression, exactly like upstream's `convertToRegexpSyntax()`.
    ///
    /// On Linux we used to use fnmatch with FNM_PATHNAME, but the windows function we used
    /// didn't have that behavior. `wildcards_match_slash` can be used to control which behavior
    /// the resulting regex shall use.
    pub fn convert_to_regexp_syntax(exclude: &str, wildcards_match_slash: bool) -> String {
        // Translate *, ?, [...] to their regex variants.
        // The escape sequences \*, \?, \[. \\ have a special meaning,
        // the other ones have already been expanded before
        // (like "\\n" being replaced by "\n").
        let chars: Vec<char> = exclude.chars().collect();
        let len = chars.len();
        let mut regex = String::new();
        let mut i = 0;
        let mut chars_to_escape = 0;
        let flush = |regex: &mut String, i: usize, chars_to_escape: &mut usize| {
            let s: String = chars[i - *chars_to_escape..i].iter().collect();
            regex.push_str(&qregex_escape(&s));
            *chars_to_escape = 0;
        };
        while i < len {
            match chars[i] {
                '*' => {
                    flush(&mut regex, i, &mut chars_to_escape);
                    regex.push_str(if wildcards_match_slash { ".*" } else { "[^/]*" });
                }
                '?' => {
                    flush(&mut regex, i, &mut chars_to_escape);
                    regex.push_str(if wildcards_match_slash { "." } else { "[^/]" });
                }
                '[' => {
                    flush(&mut regex, i, &mut chars_to_escape);
                    // Find the end of the bracket expression
                    let mut j = i + 1;
                    while j < len {
                        if chars[j] == ']' {
                            break;
                        }
                        if j != len - 1 && chars[j] == '\\' && chars[j + 1] == ']' {
                            j += 1;
                        }
                        j += 1;
                    }
                    if j == len {
                        // no matching ], just insert the escaped [
                        regex.push_str("\\[");
                    } else {
                        // Translate [! to [^
                        let mut bracket_expr: String = chars[i..=j].iter().collect();
                        if bracket_expr.starts_with("[!") {
                            bracket_expr.replace_range(1..2, "^");
                        }
                        regex.push_str(&bracket_expr);
                        i = j;
                    }
                }
                '\\' => {
                    flush(&mut regex, i, &mut chars_to_escape);
                    if i == len - 1 {
                        regex.push_str("\\\\");
                    } else {
                        // '\*' -> '\*', but '\z' -> '\\z'
                        match chars[i + 1] {
                            '*' | '?' | '[' | '\\' => {
                                regex.push_str(&qregex_escape(&chars[i + 1].to_string()));
                            }
                            _ => chars_to_escape += 2,
                        }
                        i += 1;
                    }
                }
                _ => chars_to_escape += 1,
            }
            i += 1;
        }
        flush(&mut regex, i, &mut chars_to_escape);
        regex
    }

    /// Extracts the part of a full-path pattern that can match a basename,
    /// used to trigger full-path matching during traversal (`extractBnameTrigger()`).
    pub fn extract_bname_trigger(exclude: &str, wildcards_match_slash: bool) -> String {
        // We can definitely drop everything to the left of a / - that will never match
        // any bname.
        let pattern = &exclude[exclude.rfind('/').map_or(0, |i| i + 1)..];

        // Easy case, nothing else can match a slash, so that's it.
        if !wildcards_match_slash {
            return pattern.to_string();
        }

        // Otherwise it's more complicated. Examples:
        // - "foo*bar" can match "fooX/Xbar", pattern is "*bar"
        // - "foo*bar*" can match "fooX/XbarX", pattern is "*bar*"
        // - "foo?bar" can match "foo/bar" but also "fooXbar", pattern is "*bar"

        let is_wildcard = |c: char| c == '*' || c == '?';
        let chars: Vec<char> = pattern.chars().collect();

        // First, skip wildcards on the very right of the pattern
        let mut i = chars.len() as isize - 1;
        while i >= 0 && is_wildcard(chars[i as usize]) {
            i -= 1;
        }

        // Then scan further until the next wildcard that could match a /
        while i >= 0 && !is_wildcard(chars[i as usize]) {
            i -= 1;
        }

        // Everything to the right is part of the pattern
        let mut result: String = chars[(i + 1) as usize..].iter().collect();

        // And if there was a wildcard, it starts with a *
        if i >= 0 {
            result.insert(0, '*');
        }

        result
    }

    fn clear_regexes(&mut self) {
        self.bname_traversal_regex_file.clear();
        self.bname_traversal_regex_dir.clear();
        self.full_traversal_regex_file.clear();
        self.full_traversal_regex_dir.clear();
        self.full_regex_file.clear();
        self.full_regex_dir.clear();
    }

    /// `prepare()`: rebuilds the regexes of every base path.
    fn prepare_all(&mut self) {
        // clear all regex
        self.clear_regexes();

        let keys: Vec<String> = self.all_excludes.keys().cloned().collect();
        for base_path in keys {
            self.prepare_base(&base_path);
        }
    }

    /// `prepare(basePath)`: generates optimized regular expressions for the
    /// exclude patterns anchored to `base_path`.
    ///
    /// The optimization works in two steps: First, all supported patterns are put
    /// into full_regex_file/full_regex_dir. These regexes can be applied to the full
    /// path to determine whether it is excluded or not.
    ///
    /// The second is a performance optimization. The particularly common use
    /// case for excludes during a sync run is "traversal": Instead of checking
    /// the full path every time, we check each parent path with the traversal
    /// function incrementally.
    ///
    /// Example: When the sync run eventually arrives at "a/b/c it can assume
    /// that the traversal matching has already been run on "a", "a/b"
    /// and just needs to run the traversal matcher on "a/b/c".
    ///
    /// The full matcher is equivalent to or-combining the traversal match results
    /// of all parent paths:
    ///   full("a/b/c/d") == traversal("a") || traversal("a/b") || traversal("a/b/c")
    ///
    /// The traversal matcher can be extremely fast because it has a fast early-out
    /// case: It checks the bname part of the path against bname_traversal_regex
    /// and only runs a simplified full_traversal_regex on the whole path if bname
    /// activation for it was triggered.
    ///
    /// Note: The traversal matcher will return not-excluded on some paths that the
    /// full matcher would exclude. Example: "b" is excluded. traversal("b/c")
    /// returns not-excluded because "c" isn't a bname activation pattern.
    fn prepare_base(&mut self, base_path: &str) {
        debug_assert!(self.all_excludes.contains_key(base_path));

        // Build regular expressions for the different cases.
        //
        // To compose the bname_traversal_regex, full_traversal_regex and full_regex
        // patterns we collect several subgroups of patterns here.
        //
        // * The "full" group will contain all patterns that contain a non-trailing
        //   slash. They only make sense in the full_regex and full_traversal_regex.
        // * The "bname" group contains all patterns without a non-trailing slash.
        //   These need separate handling in the full_regex (slash-containing
        //   patterns must be anchored to the front, these don't need it)
        // * The "bnameTrigger" group contains the bname part of all patterns in the
        //   "full" group. These and the "bname" group become bname_traversal_regex.
        //
        // To complicate matters, the exclude patterns have two binary attributes
        // meaning we'll end up with 4 variants:
        // * "]" patterns mean "EXCLUDE_AND_REMOVE", they get collected in the
        //   pattern strings ending in "Remove". The others go to "Keep".
        // * trailing-slash patterns match directories only. They get collected
        //   in the pattern strings saying "Dir", the others go into "FileDir"
        //   because they match files and directories.

        let mut full_file_dir_keep = String::new();
        let mut full_file_dir_remove = String::new();
        let mut full_dir_keep = String::new();
        let mut full_dir_remove = String::new();

        let mut bname_file_dir_keep = String::new();
        let mut bname_file_dir_remove = String::new();
        let mut bname_dir_keep = String::new();
        let mut bname_dir_remove = String::new();

        let mut bname_trigger_file_dir = String::new();
        let mut bname_trigger_dir = String::new();

        fn regex_append(file_dir: &mut String, dir: &mut String, append_me: &str, dir_only: bool) {
            let pattern = if dir_only { dir } else { file_dir };
            if !pattern.is_empty() {
                pattern.push('|');
            }
            pattern.push_str(append_me);
        }

        let all_values = self
            .all_excludes
            .get(base_path)
            .cloned()
            .unwrap_or_default();
        for mut exclude in all_values {
            // Note: an empty pattern behaves like upstream in release builds,
            // where QString::operator[](0) of an empty string yields '\0'.
            if exclude.starts_with('\n') {
                continue; // empty line
            }
            if exclude.starts_with('\r') {
                continue; // empty line
            }

            let match_dir_only = exclude.ends_with('/');
            if match_dir_only {
                exclude.pop();
            }

            let remove_excluded = exclude.starts_with(']');
            if remove_excluded {
                exclude.remove(0);
            }

            let full_path = exclude.contains('/');

            let (bname_file_dir, bname_dir, full_file_dir, full_dir) = if remove_excluded {
                (
                    &mut bname_file_dir_remove,
                    &mut bname_dir_remove,
                    &mut full_file_dir_remove,
                    &mut full_dir_remove,
                )
            } else {
                (
                    &mut bname_file_dir_keep,
                    &mut bname_dir_keep,
                    &mut full_file_dir_keep,
                    &mut full_dir_keep,
                )
            };

            if full_path {
                // The full pattern is matched against a path relative to local_path, however exclude is
                // relative to base_path at this point.
                // We know for sure that both local_path and base_path are absolute and that base_path is
                // contained in local_path. So we can simply remove it from the begining.
                let rel_path = base_path.get(self.local_path.len()..).unwrap_or("");
                // Make exclude relative to local_path
                exclude.insert_str(0, rel_path);
            }
            let regex_exclude =
                Self::convert_to_regexp_syntax(&exclude, self.wildcards_match_slash);
            if !full_path {
                regex_append(bname_file_dir, bname_dir, &regex_exclude, match_dir_only);
            } else {
                regex_append(full_file_dir, full_dir, &regex_exclude, match_dir_only);

                // For activation, trigger on the 'bname' part of the full pattern.
                let bname_exclude =
                    Self::extract_bname_trigger(&exclude, self.wildcards_match_slash);
                let regex_bname = Self::convert_to_regexp_syntax(&bname_exclude, true);
                regex_append(
                    &mut bname_trigger_file_dir,
                    &mut bname_trigger_dir,
                    &regex_bname,
                    match_dir_only,
                );
            }
        }

        // The empty pattern would match everything - change it to match-nothing
        for pattern in [
            &mut full_file_dir_keep,
            &mut full_file_dir_remove,
            &mut full_dir_keep,
            &mut full_dir_remove,
            &mut bname_file_dir_keep,
            &mut bname_file_dir_remove,
            &mut bname_dir_keep,
            &mut bname_dir_remove,
            &mut bname_trigger_file_dir,
            &mut bname_trigger_dir,
        ] {
            if pattern.is_empty() {
                *pattern = "a^".to_string();
            }
        }

        let ci = fs_case_preserving();
        let key = base_path.to_string();

        // The bname regex is applied to the bname only, so it must be
        // anchored in the beginning and in the end. It has the structure:
        // (exclude)|(excluderemove)|(bname triggers).
        // If the third group matches, the fullActivatedRegex needs to be applied
        // to the full path.
        self.bname_traversal_regex_file.insert(
            key.clone(),
            PatternRegex::new(
                format!(
                    "^(?P<exclude>{bname_file_dir_keep})$|\
                     ^(?P<excluderemove>{bname_file_dir_remove})$|\
                     ^(?P<trigger>{bname_trigger_file_dir})$"
                ),
                ci,
            ),
        );
        self.bname_traversal_regex_dir.insert(
            key.clone(),
            PatternRegex::new(
                format!(
                    "^(?P<exclude>{bname_file_dir_keep}|{bname_dir_keep})$|\
                     ^(?P<excluderemove>{bname_file_dir_remove}|{bname_dir_remove})$|\
                     ^(?P<trigger>{bname_trigger_file_dir}|{bname_trigger_dir})$"
                ),
                ci,
            ),
        );

        // The full traveral regex is applied to the full path if the trigger capture of
        // the bname regex matches. Its basic form is (exclude)|(excluderemove)".
        // This pattern can be much simpler than fullRegex since we can assume a traversal
        // situation and doesn't need to look for bname patterns in parent paths.
        self.full_traversal_regex_file.insert(
            key.clone(),
            PatternRegex::new(
                // Full patterns are anchored to the beginning
                format!(
                    "^(?P<exclude>{full_file_dir_keep})(?:$|/)\
                     |\
                     ^(?P<excluderemove>{full_file_dir_remove})(?:$|/)"
                ),
                ci,
            ),
        );
        self.full_traversal_regex_dir.insert(
            key.clone(),
            PatternRegex::new(
                format!(
                    "^(?P<exclude>{full_file_dir_keep}|{full_dir_keep})(?:$|/)\
                     |\
                     ^(?P<excluderemove>{full_file_dir_remove}|{full_dir_remove})(?:$|/)"
                ),
                ci,
            ),
        );

        // The full regex is applied to the full path and incorporates both bname and
        // full-path patterns. It has the form "(exclude)|(excluderemove)".
        self.full_regex_file.insert(
            key.clone(),
            PatternRegex::new(
                format!(
                    "(?P<exclude>\
                     ^(?:{full_file_dir_keep})(?:$|/)|\
                     (?:^|/)(?:{bname_file_dir_keep})(?:$|/)|\
                     (?:^|/)(?:{bname_dir_keep})/)\
                     |\
                     (?P<excluderemove>\
                     ^(?:{full_file_dir_remove})(?:$|/)|\
                     (?:^|/)(?:{bname_file_dir_remove})(?:$|/)|\
                     (?:^|/)(?:{bname_dir_remove})/)"
                ),
                ci,
            ),
        );
        self.full_regex_dir.insert(
            key,
            PatternRegex::new(
                format!(
                    "(?P<exclude>\
                     ^(?:{full_file_dir_keep}|{full_dir_keep})(?:$|/)|\
                     (?:^|/)(?:{bname_file_dir_keep}|{bname_dir_keep})(?:$|/))\
                     |\
                     (?P<excluderemove>\
                     ^(?:{full_file_dir_remove}|{full_dir_remove})(?:$|/)|\
                     (?:^|/)(?:{bname_file_dir_remove}|{bname_dir_remove})(?:$|/))"
                ),
                ci,
            ),
        );
    }
}

/// `filePath.startsWith(basePath, fsCasePreserving() ? CaseInsensitive : CaseSensitive)`,
/// returning the remainder of `file_path`.
fn strip_prefix_fs<'a>(file_path: &'a str, base_path: &str) -> Option<&'a str> {
    if !fs_case_preserving() {
        return file_path.strip_prefix(base_path);
    }
    let mut rest = file_path.char_indices();
    for b in base_path.chars() {
        let (_, f) = rest.next()?;
        if f != b && !f.to_lowercase().eq(b.to_lowercase()) {
            return None;
        }
    }
    Some(rest.as_str())
}

/// `QFileInfo(path).absolutePath()`: the absolute path of the parent directory.
fn qfileinfo_absolute_path(path: &str) -> String {
    let absolute = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{}/{}", cwd.trim_end_matches('/'), path)
    };
    match absolute.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => absolute[..idx].to_string(),
    }
}

/// `FileSystem::isReadable()`: the file exists and can be opened for reading.
fn is_readable(path: &str) -> bool {
    fs::File::open(path).is_ok()
}

/// Opens and reads an exclude file. `None` when it cannot be opened (like
/// `QFile::open()` failing, which includes directories).
fn open_and_read(path: &Path) -> Option<std::io::Result<Vec<u8>>> {
    use std::io::Read;
    if path.is_dir() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let mut content = Vec::new();
    Some(file.read_to_end(&mut content).map(|_| content))
}

/// Line iterator with `QIODevice::readLine()` semantics: lines include their
/// `\n`; a missing final newline still yields the last line; no empty line
/// is produced after a trailing `\n`.
struct QtLines<'a> {
    rest: &'a [u8],
}

impl<'a> QtLines<'a> {
    fn new(content: &'a [u8]) -> Self {
        Self { rest: content }
    }
}

impl<'a> Iterator for QtLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let end = self
            .rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(self.rest.len(), |i| i + 1);
        let (line, rest) = self.rest.split_at(end);
        self.rest = rest;
        Some(line)
    }
}
