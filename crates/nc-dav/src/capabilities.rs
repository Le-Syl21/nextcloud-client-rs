// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the parts of upstream `src/libsync/capabilities.cpp` (nextcloud/desktop
// v34.0.5) used by the sync engine.

//! Server capabilities (`Capabilities`): the `ocs/v1.php/cloud/capabilities`
//! data, read with upstream's `QVariant` conversion rules.

use serde_json::{Map, Value};

/// `QVariant::toByteArray()` of a JSON value.
fn to_byte_array(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// `QVariant::toBool()` of a JSON value.
fn to_bool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => {
            let l = s.to_lowercase();
            !(l.is_empty() || l == "0" || l == "false")
        }
        _ => false,
    }
}

/// `QVariant::toStringList()` of a JSON value.
fn to_string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(|x| to_byte_array(Some(x))).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The capabilities of the server (`Capabilities`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Capabilities {
    caps: Map<String, Value>,
}

impl Capabilities {
    /// From the `capabilities` object of the OCS reply.
    pub fn new(caps: Map<String, Value>) -> Self {
        Self { caps }
    }

    /// From any JSON value (non-objects give empty capabilities).
    pub fn from_json(v: Value) -> Self {
        match v {
            Value::Object(m) => Self::new(m),
            _ => Self::default(),
        }
    }

    pub fn raw(&self) -> &Map<String, Value> {
        &self.caps
    }

    fn section(&self, name: &str) -> Option<&Map<String, Value>> {
        self.caps.get(name).and_then(Value::as_object)
    }

    fn get(&self, section: &str, key: &str) -> Option<&Value> {
        self.section(section).and_then(|s| s.get(key))
    }

    /// `isValid()`.
    pub fn is_valid(&self) -> bool {
        !self.caps.is_empty()
    }

    /// `chunkingNg()`, overridable with `OWNCLOUD_CHUNKING_NG`.
    pub fn chunking_ng(&self) -> bool {
        match std::env::var("OWNCLOUD_CHUNKING_NG").as_deref() {
            Ok("0") => return false,
            Ok("1") => return true,
            _ => {}
        }
        to_byte_array(self.get("dav", "chunking")).as_str() >= "1.0"
    }

    /// `bulkUpload()`.
    pub fn bulk_upload(&self) -> bool {
        to_byte_array(self.get("dav", "bulkupload")).as_str() >= "1.0"
    }

    /// `filesLockAvailable()`.
    pub fn files_lock_available(&self) -> bool {
        to_byte_array(self.get("files", "locking")).as_str() >= "1.0"
    }

    /// `filesLockTypeAvailable()`.
    pub fn files_lock_type_available(&self) -> bool {
        to_byte_array(self.get("files", "api-feature-lock-type")).as_str() >= "1.0"
    }

    /// `chunkingParallelUploadDisabled()`.
    pub fn chunking_parallel_upload_disabled(&self) -> bool {
        to_bool(self.get("dav", "chunkingParallelUploadDisabled"))
    }

    /// `uploadConflictFiles()`, overridable with `OWNCLOUD_UPLOAD_CONFLICT_FILES`.
    pub fn upload_conflict_files(&self) -> bool {
        if let Some(v) = env_nonempty("OWNCLOUD_UPLOAD_CONFLICT_FILES") {
            return v.trim().parse::<i64>().unwrap_or(0) != 0;
        }
        to_bool(self.caps.get("uploadConflictFiles"))
    }

    /// `supportedChecksumTypes()`.
    ///
    /// Upstream quirk kept: the list is created with `count` default
    /// (empty) entries and the types are appended after them.
    pub fn supported_checksum_types(&self) -> Vec<Vec<u8>> {
        let types = match self.get("checksums", "supportedTypes") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut list = vec![Vec::new(); types.len()];
        for t in &types {
            list.push(to_byte_array(Some(t)).into_bytes());
        }
        list
    }

    /// `preferredUploadChecksumType()`: `OWNCLOUD_CONTENT_CHECKSUM_TYPE`, else
    /// the server's preferred type, else `SHA1`.
    pub fn preferred_upload_checksum_type(&self) -> Vec<u8> {
        if let Ok(v) = std::env::var("OWNCLOUD_CONTENT_CHECKSUM_TYPE") {
            return v.into_bytes();
        }
        match self.get("checksums", "preferredUploadType") {
            Some(v) => to_byte_array(Some(v)).into_bytes(),
            None => b"SHA1".to_vec(),
        }
    }

    /// `uploadChecksumType()`.
    pub fn upload_checksum_type(&self) -> Vec<u8> {
        let preferred = self.preferred_upload_checksum_type();
        if !preferred.is_empty() {
            return preferred;
        }
        self.supported_checksum_types()
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// `invalidFilenameRegex()`; `None` is upstream's null `QString` (key
    /// absent).
    pub fn invalid_filename_regex(&self) -> Option<String> {
        self.get("dav", "invalidFilenameRegex")
            .map(|v| to_byte_array(Some(v)))
    }

    /// `blacklistedFiles()`.
    pub fn blacklisted_files(&self) -> Vec<String> {
        to_string_list(self.get("files", "blacklisted_files"))
    }

    /// `forbiddenFilenames()`.
    pub fn forbidden_filenames(&self) -> Vec<String> {
        to_string_list(self.get("files", "forbidden_filenames"))
    }

    /// `forbiddenFilenameBasenames()`.
    pub fn forbidden_filename_basenames(&self) -> Vec<String> {
        to_string_list(self.get("files", "forbidden_filename_basenames"))
    }

    /// `forbiddenFilenameExtensions()`.
    pub fn forbidden_filename_extensions(&self) -> Vec<String> {
        to_string_list(self.get("files", "forbidden_filename_extensions"))
    }

    /// `forbiddenFilenameCharacters()`.
    pub fn forbidden_filename_characters(&self) -> Vec<String> {
        to_string_list(self.get("files", "forbidden_filename_characters"))
    }

    /// `httpErrorCodesThatResetFailingChunkedUploads()`, with the same
    /// leading default entries quirk as [`Capabilities::supported_checksum_types`].
    pub fn http_error_codes_that_reset_failing_chunked_uploads(&self) -> Vec<i32> {
        let codes = match self.get("dav", "httpErrorCodesThatResetFailingChunkedUploads") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut list = vec![0; codes.len()];
        for c in &codes {
            list.push(match c {
                Value::Number(n) => n.as_i64().unwrap_or(0) as i32,
                Value::String(s) => s.trim().parse().unwrap_or(0),
                _ => 0,
            });
        }
        list
    }

    /// `clientSideEncryptionAvailable()`. E2EE is not supported by this port;
    /// the capability is still read so encrypted folders are handled like the
    /// official client does without E2EE set up.
    pub fn client_side_encryption_available(&self) -> bool {
        let Some(props) = self.section("end-to-end-encryption") else {
            return false;
        };
        if !to_bool(props.get("enabled")) {
            return false;
        }
        let version = props
            .get("api-version")
            .map(|v| to_byte_array(Some(v)))
            .unwrap_or_else(|| "1.0".to_owned());
        version
            .split('.')
            .next()
            .is_some_and(|m| m.parse::<i32>().is_ok())
    }

    /// `clientSideEncryptionVersion()`.
    pub fn client_side_encryption_version(&self) -> f64 {
        let Some(props) = self.section("end-to-end-encryption") else {
            return 1.0;
        };
        props
            .get("api-version")
            .map(|v| to_byte_array(Some(v)))
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn derived_capability_accessors() {
        let caps = Capabilities::from_json(json!({
            "dav": {"chunking": "1.0", "bulkupload": "1.0", "invalidFilenameRegex": ""},
            "files": {"locking": "1.0", "blacklisted_files": [".htaccess"]},
            "checksums": {"supportedTypes": ["SHA1"], "preferredUploadType": "SHA1"},
        }));
        assert!(caps.is_valid());
        assert!(caps.chunking_ng());
        assert!(caps.bulk_upload());
        assert!(caps.files_lock_available());
        assert!(!caps.files_lock_type_available());
        assert_eq!(caps.blacklisted_files(), [".htaccess"]);
        assert_eq!(caps.invalid_filename_regex().as_deref(), Some(""));
        // Upstream quirk: one empty entry before the real one.
        assert_eq!(
            caps.supported_checksum_types(),
            [b"".to_vec(), b"SHA1".to_vec()]
        );
        assert!(!Capabilities::default().chunking_ng());
        assert_eq!(Capabilities::default().invalid_filename_regex(), None);
        assert_eq!(
            Capabilities::default().preferred_upload_checksum_type(),
            b"SHA1"
        );
    }
}
