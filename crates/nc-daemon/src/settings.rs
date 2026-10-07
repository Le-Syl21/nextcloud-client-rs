// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `QSettings` in `IniFormat`, as far as `nextcloud.cfg` needs it, on top of
//! the format-preserving [`ini_preserve`] parser.
//!
//! Written from the documented behaviour of `QSettings::IniFormat` and from
//! real `nextcloud.cfg` files:
//!
//! * a key is a `/`-separated path (`Accounts/0/Folders/1/localPath`); in the
//!   file, the first component is the section and the rest is the key with
//!   `\` as separator (`[Accounts]` then `0\Folders\1\localPath=...`); keys
//!   without a group live in `[General]`, and a real group called `General`
//!   is written `[%General]`;
//! * key characters other than ASCII letters, digits, `_`, `-` and `.` are
//!   escaped as `%XX` (Latin-1) or `%UXXXX` (UTF-16 unit), upper-case hex;
//! * string values are written with C-like escapes (`\\`, `\"`, `\n`,
//!   `\t`, `\xHH`, ...) and quoted when they contain `,`, `;` or `=` or start
//!   or end with a space; non-ASCII characters are written as UTF-8,
//!   except in byte arrays and variants, where they are `\xHH`;
//! * a value containing unquoted commas is a string list (`a, b, c`); an
//!   empty list is `@Invalid()`;
//! * values starting with `@` are typed: `@ByteArray(...)`, `@Variant(...)`
//!   (a `QDataStream` blob, kept opaque here), `@Invalid()`, `@String(...)`,
//!   and a string that really starts with `@` is written `@@...`;
//! * booleans and numbers are plain strings (`true`, `false`, `42`).
//!
//! Lines this module does not touch keep their exact text (comments,
//! ordering, unknown keys, `@Variant` blobs such as `serverColor`). New keys
//! are added at the end of their section; ini-preserve writes them as
//! `key = value` (with spaces), which `QSettings` reads like `key=value`.
//!
//! Like `QSettings`, writes to a file take a `<file>.lock` lock file
//! (`QLockFile` layout: pid, application name, host name) and replace the
//! file atomically.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ini_preserve::Ini;

/// Errors reading or writing a settings file.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is locked by another process ({holder})")]
    Locked { path: PathBuf, holder: String },
    #[error("these settings have no file to be saved to")]
    NoPath,
}

/// A settings value (`QVariant` as stored by `QSettings`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// `@Invalid()`, also an empty string list.
    Invalid,
    /// A string. Booleans and numbers are strings too (`"true"`, `"30000"`).
    String(String),
    /// A list of two or more strings (`a, b`).
    StringList(Vec<String>),
    /// `@ByteArray(...)`: the bytes are the Latin-1 code of the characters.
    ByteArray(Vec<u8>),
    /// `@Variant(...)`: an opaque `QDataStream` serialisation (for example a
    /// `QColor`), as bytes.
    Variant(Vec<u8>),
    /// Any other typed value (`@DateTime(...)`, `@Rect(...)`, ...), kept as
    /// its decoded text and written back unchanged.
    Other(String),
}

impl Value {
    /// `QVariant::toString()`.
    pub fn to_string_value(&self) -> String {
        match self {
            Self::String(s) | Self::Other(s) => s.clone(),
            Self::StringList(l) if l.len() == 1 => l[0].clone(),
            Self::ByteArray(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        }
    }

    /// `QVariant::toBool()`: a string is true unless it is empty, `0` or
    /// `false` (any case).
    pub fn to_bool(&self) -> bool {
        match self {
            Self::String(s) => {
                let t = s.trim();
                !(t.is_empty() || t == "0" || t.eq_ignore_ascii_case("false"))
            }
            Self::ByteArray(b) => {
                let t = String::from_utf8_lossy(b);
                let t = t.trim();
                !(t.is_empty() || t == "0" || t.eq_ignore_ascii_case("false"))
            }
            _ => false,
        }
    }

    /// `QVariant::toLongLong(&ok)`: `None` when the value is not a number.
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            Self::String(s) => parse_qt_integer(s),
            Self::ByteArray(b) => parse_qt_integer(&String::from_utf8_lossy(b)),
            _ => None,
        }
    }

    /// `QVariant::toStringList()`.
    pub fn to_string_list(&self) -> Vec<String> {
        match self {
            Self::String(s) => vec![s.clone()],
            Self::StringList(l) => l.clone(),
            _ => Vec::new(),
        }
    }

    /// `QVariant::toByteArray()`.
    pub fn to_byte_array(&self) -> Vec<u8> {
        match self {
            Self::ByteArray(b) | Self::Variant(b) => b.clone(),
            Self::String(s) => s.as_bytes().to_vec(),
            _ => Vec::new(),
        }
    }

    /// `QVariant::isValid()`.
    pub fn is_valid(&self) -> bool {
        !matches!(self, Self::Invalid)
    }
}

/// `QString::toLongLong`: optional surrounding whitespace, optional sign,
/// decimal digits.
fn parse_qt_integer(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Self {
        Self::String(s.clone())
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Self::String(if b { "true" } else { "false" }.to_owned())
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Self::String(n.to_string())
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Self::String(n.to_string())
    }
}

impl From<Vec<String>> for Value {
    fn from(l: Vec<String>) -> Self {
        Self::StringList(l)
    }
}

// ---------------------------------------------------------------------------
// Key escaping
// ---------------------------------------------------------------------------

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Escapes one key (or section) for the file: `/` becomes `\`, ASCII
/// letters, digits, `_`, `-` and `.` stay, any other UTF-16 unit is `%XX`
/// (up to 0xFF) or `%UXXXX`.
pub fn escape_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() * 3 / 2);
    for unit in key.encode_utf16() {
        match unit {
            0x2F => out.push('\\'),
            u if u < 0x80
                && ((u as u8).is_ascii_alphanumeric() || matches!(u as u8, b'_' | b'-' | b'.')) =>
            {
                out.push(u as u8 as char);
            }
            u if u <= 0xFF => {
                out.push('%');
                out.push(HEX_UPPER[(u >> 4) as usize] as char);
                out.push(HEX_UPPER[(u & 0xF) as usize] as char);
            }
            u => {
                out.push_str("%U");
                for shift in [12, 8, 4, 0] {
                    out.push(HEX_UPPER[((u >> shift) & 0xF) as usize] as char);
                }
            }
        }
    }
    out
}

/// Unescapes a key (or section) read from the file: `\` is a `/`
/// separator, `%XX` and `%UXXXX` are UTF-16 units; anything else (also a
/// malformed `%`) is kept.
pub fn unescape_key(raw: &str) -> String {
    let units: Vec<u16> = raw.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if u == u16::from(b'\\') {
            out.push(u16::from(b'/'));
            i += 1;
            continue;
        }
        if u != u16::from(b'%') || i == units.len() - 1 {
            out.push(u);
            i += 1;
            continue;
        }
        let (first, digits) = if units[i + 1] == u16::from(b'U') {
            (i + 2, 4)
        } else {
            (i + 1, 2)
        };
        if first + digits > units.len() {
            out.push(u);
            i += 1;
            continue;
        }
        let hex = String::from_utf16_lossy(&units[first..first + digits]);
        match u16::from_str_radix(&hex, 16) {
            Ok(v) if hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                out.push(v);
                i = first + digits;
            }
            _ => {
                out.push(u);
                i += 1;
            }
        }
    }
    String::from_utf16_lossy(&out)
}

/// `QSettings` key normalisation: no empty components, no leading or
/// trailing `/`.
pub fn normalize_key(key: &str) -> String {
    key.split('/')
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Joins a group and a key.
pub fn join_key(group: &str, key: &str) -> String {
    normalize_key(&format!("{group}/{key}"))
}

// ---------------------------------------------------------------------------
// Value escaping
// ---------------------------------------------------------------------------

/// Escapes one string value (`iniEscapedString` semantics).
fn escape_string_into(s: &str, out: &mut String) {
    let start = out.len();
    let mut needs_quotes = false;
    let mut escape_next_if_digit = false;
    // Byte arrays and variants are Latin-1: their characters above 0x7E are
    // written as `\xHH` instead of UTF-8.
    let use_codec = !(s.starts_with("@ByteArray(") || s.starts_with("@Variant("));
    for ch in s.chars() {
        if matches!(ch, ';' | ',' | '=') {
            needs_quotes = true;
        }
        if escape_next_if_digit && ch.is_ascii_hexdigit() {
            out.push_str(&format!("\\x{:x}", ch as u32));
            continue;
        }
        escape_next_if_digit = false;
        match ch {
            '\0' => {
                out.push_str("\\0");
                escape_next_if_digit = true;
            }
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            c if (c as u32) <= 0x1F || ((c as u32) >= 0x7F && !use_codec) => {
                out.push_str(&format!("\\x{:x}", c as u32));
                escape_next_if_digit = true;
            }
            c => out.push(c),
        }
    }
    let written = &out[start..];
    if needs_quotes || (!written.is_empty() && (written.starts_with(' ') || written.ends_with(' ')))
    {
        out.insert(start, '"');
        out.push('"');
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn to_latin1(s: &str) -> Vec<u8> {
    // Characters above U+00FF cannot come from a Latin-1 byte array; Qt's
    // `toLatin1()` turns them into `?`.
    s.chars()
        .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
        .collect()
}

/// `variantToString` for a single value.
fn variant_to_string(value: &Value) -> String {
    match value {
        Value::Invalid => "@Invalid()".to_owned(),
        Value::String(s) => {
            if s.starts_with('@') {
                format!("@{s}")
            } else {
                s.clone()
            }
        }
        Value::StringList(l) if l.len() == 1 => variant_to_string(&Value::String(l[0].clone())),
        Value::StringList(_) => String::new(),
        Value::ByteArray(b) => format!("@ByteArray({})", latin1(b)),
        Value::Variant(b) => format!("@Variant({})", latin1(b)),
        Value::Other(s) => s.clone(),
    }
}

/// The text written after `=` for a value.
pub fn escape_value(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::StringList(l) if l.is_empty() => out.push_str("@Invalid()"),
        Value::StringList(l) => {
            for (i, item) in l.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                escape_string_into(&variant_to_string(&Value::String(item.clone())), &mut out);
            }
        }
        v => escape_string_into(&variant_to_string(v), &mut out),
    }
    out
}

/// `stringToVariant`.
fn string_to_variant(s: String) -> Value {
    if s.starts_with('@') {
        if s.ends_with(')') {
            if let Some(inner) = s.strip_prefix("@ByteArray(") {
                return Value::ByteArray(to_latin1(&inner[..inner.len() - 1]));
            }
            if let Some(inner) = s.strip_prefix("@String(") {
                return Value::String(inner[..inner.len() - 1].to_owned());
            }
            if let Some(inner) = s.strip_prefix("@Variant(") {
                return Value::Variant(to_latin1(&inner[..inner.len() - 1]));
            }
            if s == "@Invalid()" {
                return Value::Invalid;
            }
            if s.starts_with("@DateTime(")
                || s.starts_with("@Rect(")
                || s.starts_with("@Size(")
                || s.starts_with("@Point(")
            {
                return Value::Other(s);
            }
        }
        if let Some(rest) = s.strip_prefix("@@") {
            return Value::String(format!("@{rest}"));
        }
    }
    Value::String(s)
}

fn chop_trailing_spaces(s: &mut String, limit: usize) {
    while s.len() > limit && (s.ends_with(' ') || s.ends_with('\t')) {
        s.pop();
    }
}

fn push_code_unit(out: &mut String, value: u32) {
    out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
}

/// Parses the text after `=` (`iniUnescapedStringList` semantics).
/// Returns the string, or the list when the text has unquoted commas.
fn unescape_string_list(text: &str) -> Result<String, Vec<String>> {
    const ESCAPES: [(char, char); 11] = [
        ('a', '\x07'),
        ('b', '\x08'),
        ('f', '\x0c'),
        ('n', '\n'),
        ('r', '\r'),
        ('t', '\t'),
        ('v', '\x0b'),
        ('"', '"'),
        ('?', '?'),
        ('\'', '\''),
        ('\\', '\\'),
    ];
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut is_list = false;
    let mut list: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let mut current_quoted = false;
    let mut cur = String::new();
    let mut chop_limit;

    let skip_spaces = |i: &mut usize| {
        while *i < n && (chars[*i] == ' ' || chars[*i] == '\t') {
            *i += 1;
        }
    };

    skip_spaces(&mut i);
    chop_limit = cur.len();
    let mut chop_at_end = true;
    'outer: while i < n {
        match chars[i] {
            '\\' => {
                i += 1;
                if i >= n {
                    chop_at_end = false;
                    break 'outer;
                }
                let c = chars[i];
                i += 1;
                if let Some(&(_, v)) = ESCAPES.iter().find(|(k, _)| *k == c) {
                    cur.push(v);
                } else if c == 'x' {
                    if i >= n {
                        chop_at_end = false;
                        break 'outer;
                    }
                    if chars[i].is_ascii_hexdigit() {
                        let mut v: u32 = 0;
                        while i < n && chars[i].is_ascii_hexdigit() {
                            v = v
                                .wrapping_mul(16)
                                .wrapping_add(chars[i].to_digit(16).unwrap_or(0));
                            i += 1;
                        }
                        push_code_unit(&mut cur, v & 0xFFFF);
                    }
                } else if let Some(o) = c.to_digit(8) {
                    let mut v = o;
                    while i < n
                        && let Some(d) = chars[i].to_digit(8)
                    {
                        v = v.wrapping_mul(8).wrapping_add(d);
                        i += 1;
                    }
                    push_code_unit(&mut cur, v & 0xFFFF);
                } else if (c == '\n' || c == '\r')
                    && i < n
                    && (chars[i] == '\n' || chars[i] == '\r')
                    && chars[i] != c
                {
                    // A line continuation: `\` then one of `\n`, `\r`,
                    // `\r\n`, `\n\r`.
                    i += 1;
                }
                // Any other escaped character is skipped.
                chop_limit = cur.len();
            }
            '"' => {
                i += 1;
                current_quoted = true;
                in_quotes = !in_quotes;
                if !in_quotes {
                    skip_spaces(&mut i);
                    chop_limit = cur.len();
                }
            }
            ',' if !in_quotes => {
                if !current_quoted {
                    chop_trailing_spaces(&mut cur, chop_limit);
                }
                is_list = true;
                list.push(std::mem::take(&mut cur));
                current_quoted = false;
                i += 1;
                skip_spaces(&mut i);
                chop_limit = 0;
            }
            _ => {
                let mut j = i + 1;
                while j < n && !matches!(chars[j], '\\' | '"' | ',') {
                    j += 1;
                }
                cur.extend(&chars[i..j]);
                i = j;
            }
        }
    }
    if chop_at_end && !current_quoted {
        chop_trailing_spaces(&mut cur, chop_limit);
    }
    if is_list {
        list.push(cur);
        Err(list)
    } else {
        Ok(cur)
    }
}

/// Decodes the text after `=` into a value.
pub fn unescape_value(text: &str) -> Value {
    match unescape_string_list(text) {
        Ok(s) => string_to_variant(s),
        Err(list) => {
            let items: Vec<Value> = list.into_iter().map(string_to_variant).collect();
            if items.iter().all(|v| matches!(v, Value::String(_))) {
                Value::StringList(
                    items
                        .into_iter()
                        .map(|v| match v {
                            Value::String(s) => s,
                            _ => String::new(),
                        })
                        .collect(),
                )
            } else {
                // A list of typed values: keep its text (only its strings are
                // used by the client).
                Value::Other(text.to_owned())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// One key of the file: where it is, its decoded path and raw value text.
#[derive(Clone, Debug)]
struct Entry {
    section: String,
    key: String,
    path: String,
    raw: String,
}

/// A `QSettings` (IniFormat) document.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    ini: Ini,
    path: Option<PathBuf>,
}

impl fmt::Display for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.ini, f)
    }
}

/// Decoded group prefix of a section header.
fn section_prefix(name: &str) -> String {
    if name.eq_ignore_ascii_case("general") {
        String::new()
    } else if name.eq_ignore_ascii_case("%general") {
        name[1..].to_owned()
    } else {
        normalize_key(&unescape_key(name))
    }
}

/// The section (raw) and key (raw) where a new key path is written.
fn placement(path: &str) -> (String, String) {
    match path.split_once('/') {
        None => ("General".to_owned(), escape_key(path)),
        Some((first, rest)) => {
            let mut section = escape_key(first);
            if section.eq_ignore_ascii_case("general") {
                section.insert(0, '%');
            }
            (section, escape_key(rest))
        }
    }
}

impl Settings {
    /// An empty document without file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses settings text.
    pub fn parse(text: &str) -> Result<Self, SettingsError> {
        let ini = Ini::parse(text).map_err(|message| SettingsError::Parse {
            path: PathBuf::new(),
            message,
        })?;
        Ok(Self { ini, path: None })
    }

    /// Loads a settings file. A missing file gives empty settings that will
    /// be saved there.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let path = path.as_ref();
        let text = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(SettingsError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let ini = Ini::parse(&text).map_err(|message| SettingsError::Parse {
            path: path.to_owned(),
            message,
        })?;
        Ok(Self {
            ini,
            path: Some(path.to_owned()),
        })
    }

    /// The file these settings are saved to.
    pub fn file_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Sets the file these settings are saved to.
    pub fn set_file_path(&mut self, path: impl Into<PathBuf>) {
        self.path = Some(path.into());
    }

    fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for section in self.ini.sections() {
            let prefix = section_prefix(section);
            for (key, raw) in self.ini.keys(section) {
                let path = join_key(&prefix, &unescape_key(key));
                if path.is_empty() {
                    continue;
                }
                out.push(Entry {
                    section: section.to_owned(),
                    key: key.to_owned(),
                    path,
                    raw: raw.to_owned(),
                });
            }
        }
        out
    }

    fn find(&self, key: &str) -> Option<Entry> {
        let key = normalize_key(key);
        self.entries().into_iter().find(|e| e.path == key)
    }

    /// `contains(key)`.
    pub fn contains(&self, key: &str) -> bool {
        self.find(key).is_some()
    }

    /// `value(key)`: `None` when the key is absent.
    pub fn value(&self, key: &str) -> Option<Value> {
        self.find(key).map(|e| unescape_value(&e.raw))
    }

    /// `value(key).toString()`, empty when absent.
    pub fn string(&self, key: &str) -> String {
        self.value(key)
            .map(|v| v.to_string_value())
            .unwrap_or_default()
    }

    /// `value(key, default).toBool()`.
    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        self.value(key).map_or(default, |v| v.to_bool())
    }

    /// `value(key, default).toLongLong()` (an unparsable value gives 0, like
    /// `QVariant::toLongLong`).
    pub fn i64_or(&self, key: &str, default: i64) -> i64 {
        self.value(key)
            .map_or(default, |v| v.to_i64().unwrap_or_default())
    }

    /// The raw (escaped) text of a value, as in the file.
    pub fn raw_value(&self, key: &str) -> Option<String> {
        self.find(key).map(|e| e.raw)
    }

    /// `setValue(key, value)`.
    pub fn set_value(&mut self, key: &str, value: impl Into<Value>) {
        let raw = escape_value(&value.into());
        self.set_raw_value(key, &raw);
    }

    /// Writes the raw (already escaped) text of a value.
    pub fn set_raw_value(&mut self, key: &str, raw: &str) {
        let key = normalize_key(key);
        if key.is_empty() {
            return;
        }
        if let Some(e) = self.find(&key) {
            self.ini.set(&e.section, &e.key, raw);
        } else {
            let (section, k) = placement(&key);
            self.ini.set(&section, &k, raw);
        }
    }

    /// `remove(key)`: the key and every key below it; an empty key clears
    /// everything.
    pub fn remove(&mut self, key: &str) {
        let key = normalize_key(key);
        let prefix = format!("{key}/");
        let doomed: Vec<Entry> = self
            .entries()
            .into_iter()
            .filter(|e| key.is_empty() || e.path == key || e.path.starts_with(&prefix))
            .collect();
        let mut sections = BTreeSet::new();
        for e in doomed {
            self.ini.remove(&e.section, &e.key);
            sections.insert(e.section);
        }
        // QSettings does not write sections left without keys.
        for s in sections {
            if self.ini.keys(&s).is_empty() {
                self.ini.remove_section(&s);
            }
        }
    }

    fn below(&self, group: &str) -> Vec<String> {
        let group = normalize_key(group);
        let prefix = if group.is_empty() {
            String::new()
        } else {
            format!("{group}/")
        };
        self.entries()
            .into_iter()
            .filter_map(|e| e.path.strip_prefix(&prefix).map(str::to_owned))
            .collect()
    }

    /// `childGroups()` of a group (sorted, like `QSettings`).
    pub fn child_groups(&self, group: &str) -> Vec<String> {
        let set: BTreeSet<String> = self
            .below(group)
            .into_iter()
            .filter_map(|rest| rest.split_once('/').map(|(g, _)| g.to_owned()))
            .collect();
        set.into_iter().collect()
    }

    /// `childKeys()` of a group (sorted).
    pub fn child_keys(&self, group: &str) -> Vec<String> {
        let set: BTreeSet<String> = self
            .below(group)
            .into_iter()
            .filter(|rest| !rest.contains('/'))
            .collect();
        set.into_iter().collect()
    }

    /// `allKeys()` of a group, relative to it (sorted).
    pub fn all_keys(&self, group: &str) -> Vec<String> {
        let set: BTreeSet<String> = self.below(group).into_iter().collect();
        set.into_iter().collect()
    }

    /// Copies every key below `from` (raw text, so typed values survive) to
    /// below `to`.
    pub fn copy_group_raw(&self, from: &str, target: &mut Settings, to: &str) {
        for key in self.all_keys(from) {
            if let Some(raw) = self.raw_value(&join_key(from, &key)) {
                target.set_raw_value(&join_key(to, &key), &raw);
            }
        }
    }

    /// Writes the settings to their file, atomically and under the
    /// `QLockFile`-style lock.
    pub fn save(&self) -> Result<(), SettingsError> {
        let path = self.path.clone().ok_or(SettingsError::NoPath)?;
        let _lock = LockFile::acquire(&path)?;
        self.write_unlocked(&path)
    }

    /// Re-reads the file under its lock, applies `f`, and writes it back:
    /// what another process wrote in between is kept (the counterpart of
    /// `QSettings::sync()` merging).
    pub fn modify<T>(
        path: &Path,
        f: impl FnOnce(&mut Settings) -> T,
    ) -> Result<(T, Settings), SettingsError> {
        let _lock = LockFile::acquire(path)?;
        let mut settings = Settings::load(path)?;
        let out = f(&mut settings);
        settings.write_unlocked(path)?;
        Ok((out, settings))
    }

    /// Writes the settings to `path` atomically, without taking the lock:
    /// for callers that already hold a [`LockFile`] on it.
    pub fn write_locked(&self, path: &Path) -> Result<(), SettingsError> {
        self.write_unlocked(path)
    }

    fn write_unlocked(&self, path: &Path) -> Result<(), SettingsError> {
        let werr = |source| SettingsError::Write {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(werr)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".ncsyncd-tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut file = std::fs::File::create(&tmp).map_err(werr)?;
            file.write_all(self.ini.to_string().as_bytes())
                .map_err(werr)?;
            file.sync_all().map_err(werr)?;
        }
        let perms = match std::fs::metadata(path) {
            Ok(m) => m.permissions(),
            Err(_) => {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::Permissions::from_mode(0o600)
            }
        };
        std::fs::set_permissions(&tmp, perms).map_err(werr)?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            werr(e)
        })
    }
}

// ---------------------------------------------------------------------------
// Lock file
// ---------------------------------------------------------------------------

/// A `QLockFile`-compatible lock (`<file>.lock`, created exclusively,
/// holding `pid\nappname\nhostname\n`). A lock whose process is gone, or
/// older than 30 s (`QLockFile`'s default stale time), is stale and taken
/// over.
#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
}

impl LockFile {
    const STALE: Duration = Duration::from_secs(30);
    const WAIT: Duration = Duration::from_secs(5);

    /// Locks `<file>.lock`, waiting up to 5 s for another holder.
    pub fn acquire(file: &Path) -> Result<Self, SettingsError> {
        let mut lock = file.as_os_str().to_owned();
        lock.push(".lock");
        let path = PathBuf::from(lock);
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        let started = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let host =
                        std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
                    let _ = write!(f, "{}\nncsyncd\n{}\n", std::process::id(), host.trim());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::is_stale(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() >= Self::WAIT {
                        let holder = std::fs::read_to_string(&path)
                            .unwrap_or_default()
                            .lines()
                            .take(2)
                            .collect::<Vec<_>>()
                            .join(" ");
                        return Err(SettingsError::Locked {
                            path: file.to_owned(),
                            holder,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(source) => {
                    return Err(SettingsError::Write { path, source });
                }
            }
        }
    }

    fn is_stale(path: &Path) -> bool {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let pid = content
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<u32>().ok());
        let alive = pid.is_some_and(|p| Path::new(&format!("/proc/{p}")).exists());
        if !alive {
            return true;
        }
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > Self::STALE)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests;
