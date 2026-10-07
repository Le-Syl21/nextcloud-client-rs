// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/cmd/netrcparser.{h,cpp}` (nextcloud/desktop v34.0.5).

//! `NetrcParser`: reads login and password from `~/.netrc`.

use std::collections::HashMap;

/// `NetrcParser::LoginPair`: (login, password).
pub type LoginPair = (String, String);

/// `NetrcParser`.
pub struct NetrcParser {
    netrc_location: String,
    entries: HashMap<String, LoginPair>,
    default: LoginPair,
}

impl NetrcParser {
    /// `NetrcParser(file)`: `~/.netrc` when `file` is `None`.
    pub fn new(file: Option<&str>) -> Self {
        let netrc_location = match file {
            Some(f) if !f.is_empty() => f.to_owned(),
            _ => format!("{}/.netrc", std::env::var("HOME").unwrap_or_default()),
        };
        Self {
            netrc_location,
            entries: HashMap::new(),
            default: (String::new(), String::new()),
        }
    }

    fn try_add_entry_and_clear(
        &mut self,
        machine: &mut String,
        pair: &mut LoginPair,
        is_default: &mut bool,
    ) {
        if *is_default {
            self.default = pair.clone();
        } else if !machine.is_empty() && !pair.0.is_empty() {
            self.entries.insert(machine.clone(), pair.clone());
        }
        *pair = (String::new(), String::new());
        machine.clear();
        *is_default = false;
    }

    /// `parse()`: false when the file is missing, empty or has no entry.
    pub fn parse(&mut self) -> bool {
        let Ok(content) = std::fs::read_to_string(&self.netrc_location) else {
            return false;
        };
        if content.is_empty() {
            return false;
        }
        // QString::split(QRegularExpression("\\s+")): leading whitespace gives
        // an empty first token.
        let mut tokens: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut in_whitespace = false;
        for ch in content.chars() {
            if ch.is_whitespace() {
                if !in_whitespace {
                    tokens.push(std::mem::take(&mut current));
                    in_whitespace = true;
                }
            } else {
                current.push(ch);
                in_whitespace = false;
            }
        }
        tokens.push(current);
        let mut pair: LoginPair = (String::new(), String::new());
        let mut machine = String::new();
        let mut is_default = false;
        let mut i = 0;
        while i < tokens.len() {
            let key = tokens[i].as_str();
            if key == "default" {
                self.try_add_entry_and_clear(&mut machine, &mut pair, &mut is_default);
                is_default = true;
                i += 1;
                continue; // don't read a value
            }
            i += 1;
            if i >= tokens.len() {
                log::debug!("error fetching value for {key}");
                break;
            }
            let value = tokens[i].as_str();
            match key {
                "machine" => {
                    self.try_add_entry_and_clear(&mut machine, &mut pair, &mut is_default);
                    machine = value.to_owned();
                }
                "login" => pair.0 = value.to_owned(),
                "password" => pair.1 = value.to_owned(),
                _ => {} // ignore unsupported tokens
            }
            i += 1;
        }
        self.try_add_entry_and_clear(&mut machine, &mut pair, &mut is_default);
        !self.entries.is_empty() || self.default != (String::new(), String::new())
    }

    /// `find(machine)`: the entry of `machine`, else the default one.
    pub fn find(&self, machine: &str) -> LoginPair {
        self.entries
            .get(machine)
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }
}

// Port of upstream `test/testnetrcparser.cpp` (CC0-1.0, SPDX-FileCopyrightText:
// 2024 Nextcloud GmbH and Nextcloud contributors, 2016 ownCloud GmbH).
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &std::path::Path, name: &str, content: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn pair(a: &str, b: &str) -> LoginPair {
        (a.to_owned(), b.to_owned())
    }

    #[test]
    fn test_valid_netrc() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(
            dir.path(),
            "netrctest",
            "machine foo login bar password baz\n\
             machine broken login bar2 dontbelonghere password baz2 extratokens dontcare andanother\n\
             machine\nfunnysplit\tlogin bar3 password baz3\n\
             machine frob login \"user with spaces\" password 'space pwd'\n",
        );
        let mut parser = NetrcParser::new(Some(&f));
        assert!(parser.parse());
        assert_eq!(parser.find("foo"), pair("bar", "baz"));
        assert_eq!(parser.find("broken"), pair("bar2", ""));
        assert_eq!(parser.find("funnysplit"), pair("bar3", "baz3"));
        // QEXPECT_FAIL: "Current implementation do not support spaces in username or password"
        assert_ne!(parser.find("frob"), pair("user with spaces", "space pwd"));
    }

    #[test]
    fn test_empty_netrc() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path(), "netrctestEmpty", "");
        let mut parser = NetrcParser::new(Some(&f));
        assert!(!parser.parse());
        assert_eq!(parser.find("foo"), pair("", ""));
    }

    #[test]
    fn test_valid_netrc_with_default() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(
            dir.path(),
            "netrctestDefault",
            "machine foo login bar password baz\ndefault login user password pass\n",
        );
        let mut parser = NetrcParser::new(Some(&f));
        assert!(parser.parse());
        assert_eq!(parser.find("foo"), pair("bar", "baz"));
        assert_eq!(parser.find("dontknow"), pair("user", "pass"));
    }

    #[test]
    fn test_invalid_netrc() {
        let mut parser = NetrcParser::new(Some("/invalid"));
        assert!(!parser.parse());
    }
}
