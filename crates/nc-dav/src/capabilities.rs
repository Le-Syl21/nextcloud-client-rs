// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the parts of upstream `src/libsync/capabilities.cpp` (nextcloud/desktop
// v34.0.5) used by the sync engine and covered by upstream
// `test/testcapabilities.cpp`.

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

/// `QVariant::toInt()` of a JSON value.
fn to_int(v: Option<&Value>) -> i32 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.round() as i64))
            .unwrap_or(0) as i32,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(b)) => i32::from(*b),
        _ => 0,
    }
}

/// `QVariant::toMap()` of a JSON value (non-objects give an empty map).
fn to_map(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

/// `PushNotificationType` / `PushNotificationTypes` of `capabilities.h`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushNotificationTypes(u8);

impl PushNotificationTypes {
    pub const NONE: Self = Self(0);
    pub const FILES: Self = Self(1);
    pub const ACTIVITIES: Self = Self(2);
    pub const NOTIFICATIONS: Self = Self(4);

    /// `QFlags::testFlag()`.
    pub fn test_flag(self, flag: Self) -> bool {
        if flag.0 == 0 {
            self.0 == 0
        } else {
            self.0 & flag.0 == flag.0
        }
    }

    /// `QFlags::setFlag()`.
    pub fn set_flag(&mut self, flag: Self) {
        self.0 |= flag.0;
    }
}

/// The parts of `QMimeType` that [`Capabilities::file_actions_by_mime_type`]
/// uses: the canonical name and every ancestor (`allAncestors()`, including
/// the implicit `text/plain` of `text/*` and `application/octet-stream`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MimeType {
    pub name: String,
    pub ancestors: Vec<String>,
}

impl MimeType {
    /// `QMimeType::inherits()`: true for the type itself and its ancestors.
    pub fn inherits(&self, mime_type_name: &str) -> bool {
        self.name == mime_type_name || self.ancestors.iter().any(|a| a == mime_type_name)
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

    /// `isClientStatusReportingEnabled()`: `security_guard.diagnostics`.
    pub fn is_client_status_reporting_enabled(&self) -> bool {
        let Some(security_guard) = self.caps.get("security_guard") else {
            return false;
        };
        to_bool(security_guard.get("diagnostics"))
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

    /// `shareDefaultPermissions()`: 0 when the server does not send them.
    pub fn share_default_permissions(&self) -> i32 {
        match self.get("files_sharing", "default_permissions") {
            Some(v) => to_int(Some(v)),
            None => 0,
        }
    }

    /// `userStatus()`.
    pub fn user_status(&self) -> bool {
        if !self.caps.contains_key("user_status") {
            return false;
        }
        to_bool(self.get("user_status", "enabled"))
    }

    /// `userStatusSupportsEmoji()`.
    pub fn user_status_supports_emoji(&self) -> bool {
        if !self.user_status() {
            return false;
        }
        to_bool(self.get("user_status", "supports_emoji"))
    }

    /// `userStatusSupportsBusy()`.
    pub fn user_status_supports_busy(&self) -> bool {
        if !self.user_status() {
            return false;
        }
        to_bool(self.get("user_status", "supports_busy"))
    }

    /// `maxChunkSize()`: `files.chunked_upload.max_size` (0 if unset).
    pub fn max_chunk_size(&self) -> i64 {
        let chunked = to_map(self.get("files", "chunked_upload"));
        match chunked.get("max_size") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// `maxConcurrentChunkUploads()`: `files.chunked_upload.max_parallel_count`.
    pub fn max_concurrent_chunk_uploads(&self) -> i32 {
        let chunked = to_map(self.get("files", "chunked_upload"));
        to_int(chunked.get("max_parallel_count"))
    }

    /// `availablePushNotifications() & PushNotificationType::Files`.
    pub fn push_notifications_files_available(&self) -> bool {
        self.available_push_notifications()
            .test_flag(PushNotificationTypes::FILES)
    }

    /// `availablePushNotifications()`.
    pub fn available_push_notifications(&self) -> PushNotificationTypes {
        if !self.caps.contains_key("notify_push") {
            return PushNotificationTypes::NONE;
        }
        let types = to_string_list(self.get("notify_push", "type"));
        let has = |t: &str| types.iter().any(|x| x == t);
        let mut push_notification_types = PushNotificationTypes::NONE;
        if has("files") {
            push_notification_types.set_flag(PushNotificationTypes::FILES);
        }
        if has("activities") {
            push_notification_types.set_flag(PushNotificationTypes::ACTIVITIES);
        }
        if has("notifications") {
            push_notification_types.set_flag(PushNotificationTypes::NOTIFICATIONS);
        }
        push_notification_types
    }

    /// `pushNotificationsWebSocketUrl()`, as text.
    pub fn push_notifications_web_socket_url(&self) -> String {
        let endpoints = to_map(self.get("notify_push", "endpoints"));
        to_byte_array(endpoints.get("websocket"))
    }

    /// `serverHasValidSubscription()`.
    pub fn server_has_valid_subscription(&self) -> bool {
        to_bool(self.get("support", "hasValidSubscription"))
    }

    /// `desktopEnterpriseChannel()`. Upstream falls back to
    /// `ConfigFile().defaultUpdateChannel()` (updater configuration, not
    /// ported) when the server sends none; that case is `None` here.
    pub fn desktop_enterprise_channel(&self) -> Option<String> {
        self.get("support", "desktopEnterpriseChannel")
            .map(|v| to_byte_array(Some(v)))
    }

    /// `serverHasClientIntegration()`.
    pub fn server_has_client_integration(&self) -> bool {
        !to_map(self.caps.get("client_integration")).is_empty()
    }

    /// `fileActionsByMimeType()`: the `context-menu` entries of every
    /// `client_integration` app whose `mimetype_filters` match the type.
    ///
    /// Upstream quirk kept: a filter ending with `/` (e.g. `text/`) has an
    /// empty alias, which every type name "starts with", so it matches all
    /// types; and an entry is added once per matching filter.
    pub fn file_actions_by_mime_type(&self, file_mime_type: &MimeType) -> Vec<Map<String, Value>> {
        let file_actions_map = to_map(self.caps.get("client_integration"));
        // `QVariantMap` iterates in key order.
        let mut apps: Vec<(&String, &Value)> = file_actions_map.iter().collect();
        apps.sort_by(|a, b| a.0.cmp(b.0));
        let mut context_menu_map_list: Vec<Value> = Vec::new();
        for (_, file_context_menu) in apps {
            let file_context_menu_map = to_map(Some(file_context_menu));
            let Some(context_menu) = file_context_menu_map.get("context-menu") else {
                continue;
            };
            if let Value::Array(a) = context_menu {
                context_menu_map_list.extend(a.iter().cloned());
            }
        }

        if context_menu_map_list.is_empty() {
            log::debug!(target: "nextcloud.sync.server.capabilities", "There is no context menu available in the capabilities.");
            return Vec::new();
        }

        let file_mime_type_name = file_mime_type.name.as_str();
        let mut file_actions_by_mime_type = Vec::new();
        for context_menu in &context_menu_map_list {
            let context_menu_map = to_map(Some(context_menu));
            let mimetype_filters = to_byte_array(context_menu_map.get("mimetype_filters"));
            let files_mime_type_filter_list: Vec<&str> = mimetype_filters
                .split(',')
                .filter(|s| !s.is_empty())
                .collect();

            if files_mime_type_filter_list.is_empty() {
                file_actions_by_mime_type.push(context_menu_map);
                continue;
            }

            for mime_type in files_mime_type_filter_list {
                let capabilities_mime_type = mime_type.trim();
                let mime_type_alias = capabilities_mime_type.split('/').next_back().unwrap_or("");

                if !capabilities_mime_type.is_empty()
                    && !file_mime_type_name.starts_with(capabilities_mime_type)
                    && !file_mime_type_name.contains(capabilities_mime_type)
                    && !file_mime_type.inherits(capabilities_mime_type)
                    && !file_mime_type_name.starts_with(mime_type_alias)
                    && !file_mime_type_name.contains(mime_type_alias)
                    && !file_mime_type.inherits(mime_type_alias)
                {
                    continue;
                }

                file_actions_by_mime_type.push(context_menu_map.clone());
            }
        }

        file_actions_by_mime_type
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
