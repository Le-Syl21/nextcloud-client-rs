/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-FileCopyrightText: 2020 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! `PathComponents`: a relative path split on `/`, empty parts skipped.

/// `PathComponents` (a `QStringList` subclass upstream).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathComponents(pub Vec<String>);

impl PathComponents {
    pub fn new(path: &str) -> Self {
        Self(
            path.split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    pub fn parent_dir_components(&self) -> Self {
        Self(self.0[..self.0.len().saturating_sub(1)].to_vec())
    }

    pub fn sub_components(&self) -> Self {
        Self(self.0.get(1..).unwrap_or_default().to_vec())
    }

    pub fn path_root(&self) -> &str {
        self.0.first().map_or("", String::as_str)
    }

    pub fn file_name(&self) -> &str {
        self.0.last().map_or("", String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl From<&str> for PathComponents {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<&String> for PathComponents {
    fn from(path: &String) -> Self {
        Self::new(path)
    }
}

impl From<String> for PathComponents {
    fn from(path: String) -> Self {
        Self::new(&path)
    }
}

impl From<Vec<String>> for PathComponents {
    fn from(parts: Vec<String>) -> Self {
        Self(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_navigate() {
        let p = PathComponents::new("/A//b/c.txt");
        assert_eq!(p.as_slice(), ["A", "b", "c.txt"]);
        assert_eq!(p.path_root(), "A");
        assert_eq!(p.file_name(), "c.txt");
        assert_eq!(p.parent_dir_components().as_slice(), ["A", "b"]);
        assert_eq!(p.sub_components().as_slice(), ["b", "c.txt"]);
        assert!(PathComponents::new("").is_empty());
        assert_eq!(PathComponents::new("").file_name(), "");
    }
}
