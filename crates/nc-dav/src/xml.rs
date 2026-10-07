// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the XML handling of upstream `src/libsync/networkjobs.cpp`
// (`LsColXMLParser`, `RequestEtagJob::finished`, `PropfindJob`) and
// `src/libsync/abstractnetworkjob.cpp` (`extractErrorMessage`,
// `extractException`) of nextcloud/desktop v34.0.5.

//! WebDAV XML parsing.
//!
//! Upstream parses with `QXmlStreamReader` (and `QDomDocument` for
//! `PropfindJob`). Here the documents are tokenized with `quick-xml` into the
//! same token stream `QXmlStreamReader` produces (start element with
//! namespace URI and local name, end element, character data with entities
//! resolved and CDATA included, empty elements reported as start + end), and
//! the upstream algorithms run on that stream.

use std::collections::BTreeMap;

use percent_encoding::percent_decode;
use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

const DAV: &str = "DAV:";

/// One token of the stream (`QXmlStreamReader::TokenType`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Start { ns: String, name: String },
    End { ns: String, name: String },
    Characters(String),
}

/// Error from the tokenizer (`QXmlStreamReader::hasError()`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("XML error: {0}")]
pub struct XmlError(pub String);

fn resolve_ns(r: ResolveResult<'_>) -> String {
    match r {
        ResolveResult::Bound(ns) => ns.as_ref().to_owned(),
        // `addExtraNamespaceDeclaration("d", "DAV:")`: the `d` prefix is
        // DAV: even when the document forgot to declare it.
        ResolveResult::Unknown(p) if p == "d" => DAV.to_owned(),
        _ => String::new(),
    }
}

fn predefined_entity(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        "quot" => Some('"'),
        _ => None,
    }
}

/// Tokenizes a whole document. On error the tokens read so far are returned
/// together with the error, like `QXmlStreamReader` which reports the tokens
/// before failing.
fn tokenize(xml: &[u8]) -> (Vec<Token>, Option<XmlError>) {
    let mut tokens = Vec::new();
    let Ok(text) = std::str::from_utf8(xml) else {
        return (tokens, Some(XmlError("invalid UTF-8".to_owned())));
    };
    let mut reader = NsReader::from_str(text);
    let config = reader.config_mut();
    config.expand_empty_elements = true;
    config.check_end_names = true;
    let mut depth = 0usize;
    let mut seen_root = false;
    loop {
        let (ns, event) = match reader.read_resolved_event() {
            Ok((ns, ev)) => (resolve_ns(ns), ev),
            Err(e) => return (tokens, Some(XmlError(e.to_string()))),
        };
        match event {
            Event::Start(e) => {
                if depth == 0 && seen_root {
                    return (
                        tokens,
                        Some(XmlError("extra content at end of document".into())),
                    );
                }
                seen_root = true;
                depth += 1;
                tokens.push(Token::Start {
                    ns,
                    name: e.local_name().as_ref().to_owned(),
                });
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                tokens.push(Token::End {
                    ns,
                    name: e.local_name().as_ref().to_owned(),
                });
            }
            Event::Text(t) => {
                let s = t.xml_content(XmlVersion::Implicit1_0).into_owned();
                if depth == 0 {
                    if !s.trim().is_empty() {
                        return (
                            tokens,
                            Some(XmlError("text outside of the root element".into())),
                        );
                    }
                    continue;
                }
                tokens.push(Token::Characters(s));
            }
            Event::CData(t) => {
                if depth > 0 {
                    tokens.push(Token::Characters(t.into_inner().into_owned()));
                }
            }
            Event::GeneralRef(r) => {
                let c = match r.resolve_char_ref() {
                    Ok(Some(c)) => c,
                    Ok(None) => match predefined_entity(&r) {
                        Some(c) => c,
                        None => {
                            return (
                                tokens,
                                Some(XmlError(format!("undefined entity &{};", &*r))),
                            );
                        }
                    },
                    Err(e) => return (tokens, Some(XmlError(e.to_string()))),
                };
                if depth > 0 {
                    tokens.push(Token::Characters(c.to_string()));
                }
            }
            Event::Eof => {
                if depth > 0 {
                    return (tokens, Some(XmlError("premature end of document".into())));
                }
                if !seen_root {
                    return (tokens, Some(XmlError("premature end of document".into())));
                }
                return (tokens, None);
            }
            Event::Empty(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_) => {}
        }
    }
}

/// A cursor over the token stream with the `QXmlStreamReader` helpers the
/// upstream code uses.
struct Cursor<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn next(&mut self) -> Option<&'a Token> {
        let t = self.tokens.get(self.pos);
        self.pos += 1;
        t
    }

    /// `readElementText()` after a start element: the text up to the
    /// matching end element (child elements are skipped, their text kept,
    /// as with `IncludeChildElements`; upstream only calls it on leaves).
    fn read_element_text(&mut self) -> String {
        let mut text = String::new();
        let mut depth = 0usize;
        while let Some(t) = self.next() {
            match t {
                Token::Characters(s) => text.push_str(s),
                Token::Start { .. } => depth += 1,
                Token::End { .. } => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
            }
        }
        text
    }

    /// `readContentsAsString()` of `networkjobs.cpp`: the content of the
    /// current element with child tags rendered as `<name>`/`</name>`.
    /// Returns the content and the local name of the closing element.
    fn read_contents_as_string(&mut self) -> String {
        let mut result = String::new();
        let mut level = 0i32;
        while let Some(t) = self.next() {
            match t {
                Token::Start { name, .. } => {
                    level += 1;
                    result.push('<');
                    result.push_str(name);
                    result.push('>');
                }
                Token::Characters(s) => result.push_str(s),
                Token::End { name, .. } => {
                    level -= 1;
                    if level < 0 {
                        break;
                    }
                    result.push_str("</");
                    result.push_str(name);
                    result.push('>');
                }
            }
        }
        result
    }
}

/// `QUrl::fromLocalFile(QUrl::fromPercentEncoding(href)).adjusted(NormalizePathSegments).path()`:
/// percent-decodes, then removes redundant `/` and resolves `.` and `..`
/// segments.
pub fn normalize_href(href: &str) -> String {
    let decoded = percent_decode(href.as_bytes())
        .decode_utf8_lossy()
        .into_owned();
    let absolute = decoded.starts_with('/');
    let trailing = decoded.ends_with('/') && decoded.len() > 1;
    let mut out: Vec<&str> = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut path = String::new();
    if absolute {
        path.push('/');
    }
    path.push_str(&out.join("/"));
    if trailing && !path.ends_with('/') {
        path.push('/');
    }
    path
}

/// Properties of one `<d:response>`: local property name to content, sorted
/// by name like upstream's `QMap<QString, QString>`.
pub type PropertyMap = BTreeMap<String, String>;

/// One `<d:response>` of a multistatus (`directoryListingIterated`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingEntry {
    /// The decoded href path, without trailing slash.
    pub href: String,
    /// The properties of the propstat with status 200.
    pub properties: PropertyMap,
}

/// Result of [`parse_lscol`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LsColListing {
    /// In document order; upstream emits these as they are parsed.
    pub entries: Vec<ListingEntry>,
    /// Hrefs whose resourcetype is a collection (`directoryListingSubfolders`).
    pub subfolders: Vec<String>,
}

/// Why [`parse_lscol`] failed. Upstream emits the entries parsed before the
/// failure anyway; they are kept in `partial`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LsColError {
    pub message: String,
    pub partial: LsColListing,
}

/// `LsColXMLParser::parse`: parses a PROPFIND multistatus. `expected_path`
/// is the decoded path of the request URL; every href must start with it.
pub fn parse_lscol(xml: &[u8], expected_path: &str) -> Result<LsColListing, LsColError> {
    let (tokens, xml_error) = tokenize(xml);
    let mut listing = LsColListing::default();
    let mut cur = Cursor {
        tokens: &tokens,
        pos: 0,
    };
    let mut current_href = String::new();
    let mut current_tmp_properties = PropertyMap::new();
    let mut current_http200_properties = PropertyMap::new();
    let mut current_props_have_http200 = false;
    let mut inside_propstat = false;
    let mut inside_prop = false;
    let mut inside_multistatus = false;

    while let Some(token) = cur.next() {
        if let Token::Start { ns, name } = token {
            if ns == DAV {
                match name.as_str() {
                    "href" => {
                        let href_string = normalize_href(&cur.read_element_text());
                        if !href_string.starts_with(expected_path) {
                            log::warn!(
                                target: "nextcloud.sync.networkjob.lscol",
                                "Invalid href {href_string:?} expected starting with {expected_path:?}"
                            );
                            return Err(LsColError {
                                message: "Invalid href".to_owned(),
                                partial: listing,
                            });
                        }
                        current_href = href_string;
                    }
                    "response" => {}
                    "propstat" => inside_propstat = true,
                    "status" if inside_propstat => {
                        let http_status = cur.read_element_text();
                        current_props_have_http200 = http_status.starts_with("HTTP/1.1 200");
                    }
                    "prop" => {
                        inside_prop = true;
                        continue;
                    }
                    "multistatus" => {
                        inside_multistatus = true;
                        continue;
                    }
                    _ => {}
                }
            }
            if inside_propstat && inside_prop {
                // All those elements are properties
                let property_content = cur.read_contents_as_string();
                if name == "resourcetype" && property_content.contains("collection") {
                    listing.subfolders.push(current_href.clone());
                }
                current_tmp_properties.insert(name.clone(), property_content);
            }
            continue;
        }
        if let Token::End { ns, name } = token
            && ns == DAV
        {
            match name.as_str() {
                "response" => {
                    let mut href = std::mem::take(&mut current_href);
                    if href.ends_with('/') {
                        href.pop();
                    }
                    listing.entries.push(ListingEntry {
                        href,
                        properties: std::mem::take(&mut current_http200_properties),
                    });
                }
                "propstat" => {
                    inside_propstat = false;
                    if current_props_have_http200 {
                        current_http200_properties = current_tmp_properties.clone();
                    }
                    current_tmp_properties.clear();
                    current_props_have_http200 = false;
                }
                "prop" => inside_prop = false,
                _ => {}
            }
        }
    }

    if let Some(e) = xml_error {
        log::warn!(target: "nextcloud.sync.networkjob.lscol", "ERROR {}", e.0);
        return Err(LsColError {
            message: e.0,
            partial: listing,
        });
    }
    if !inside_multistatus {
        log::warn!(target: "nextcloud.sync.networkjob.lscol", "ERROR no WebDAV response?");
        return Err(LsColError {
            message: "no WebDAV response".to_owned(),
            partial: listing,
        });
    }
    Ok(listing)
}

/// `parseEtag()` of `helpers.cpp`: strips `W/`, `-gzip` and quotes.
pub fn parse_etag(header: &[u8]) -> Vec<u8> {
    let mut result = header.to_vec();
    if result.starts_with(b"W/") {
        result.drain(..2);
    }
    // https://github.com/owncloud/client/issues/1195
    while let Some(pos) = result.windows(5).position(|w| w == b"-gzip") {
        result.drain(pos..pos + 5);
    }
    if result.len() >= 2 && result.first() == Some(&b'"') && result.last() == Some(&b'"') {
        result = result[1..result.len() - 1].to_vec();
    }
    result
}

/// `RequestEtagJob::finished` for a 207 reply: the concatenation of the
/// parsed `<d:getetag>` values.
pub fn parse_request_etag(xml: &[u8]) -> Vec<u8> {
    let (tokens, _) = tokenize(xml);
    let mut cur = Cursor {
        tokens: &tokens,
        pos: 0,
    };
    let mut etag = Vec::new();
    while let Some(t) = cur.next() {
        if let Token::Start { ns, name } = t
            && ns == DAV
            && name == "getetag"
        {
            let etag_text = cur.read_element_text();
            let parsed = parse_etag(etag_text.as_bytes());
            if parsed.is_empty() {
                etag.extend_from_slice(etag_text.as_bytes());
            } else {
                etag.extend_from_slice(&parsed);
            }
        }
    }
    etag
}

/// `PropfindJob::processPropfindDomDocument`: every child element of every
/// `prop` element (any namespace, any depth) mapped to its text. Later
/// values win. Returns an error when the document is not well-formed.
pub fn parse_propfind(xml: &[u8]) -> Result<PropertyMap, XmlError> {
    let (tokens, err) = tokenize(xml);
    if let Some(e) = err {
        return Err(e);
    }
    let mut items = PropertyMap::new();
    let mut i = 0;
    while i < tokens.len() {
        if let Token::Start { name, .. } = &tokens[i]
            && name == "prop"
        {
            // Walk the direct children of this prop element.
            let mut depth = 0usize;
            let mut j = i + 1;
            let mut child: Option<(String, String)> = None;
            while j < tokens.len() {
                match &tokens[j] {
                    Token::Start { name, .. } => {
                        if depth == 0 {
                            child = Some((name.clone(), String::new()));
                        }
                        depth += 1;
                    }
                    Token::Characters(s) => {
                        if let Some((_, text)) = child.as_mut() {
                            text.push_str(s);
                        }
                    }
                    Token::End { .. } => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                        if depth == 0
                            && let Some((k, v)) = child.take()
                        {
                            items.insert(k, v);
                        }
                    }
                }
                j += 1;
            }
        }
        i += 1;
    }
    Ok(items)
}

/// `extractErrorMessage()`: the `<s:message>` of a Sabre error document, or
/// its `<s:exception>` when there is no message.
pub fn extract_error_message(error_response: &[u8]) -> String {
    let (tokens, _) = tokenize(error_response);
    let mut cur = Cursor {
        tokens: &tokens,
        pos: 0,
    };
    match cur.next() {
        Some(Token::Start { name, .. }) if name == "error" => {}
        _ => return String::new(),
    }
    let mut exception = String::new();
    while let Some(t) = cur.next() {
        if let Token::Start { name, .. } = t {
            if name == "message" {
                let message = cur.read_element_text();
                if !message.is_empty() {
                    return message;
                }
            } else if name == "exception" {
                exception = cur.read_element_text();
            }
        }
    }
    exception
}

/// `extractException()`: the `<s:exception>` of a Sabre error document.
pub fn extract_exception(error_response: &[u8]) -> String {
    let (tokens, _) = tokenize(error_response);
    let mut cur = Cursor {
        tokens: &tokens,
        pos: 0,
    };
    match cur.next() {
        Some(Token::Start { name, .. }) if name == "error" => {}
        _ => return String::new(),
    }
    while let Some(t) = cur.next() {
        if let Token::Start { name, .. } = t
            && name == "exception"
        {
            return cur.read_element_text();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests;
