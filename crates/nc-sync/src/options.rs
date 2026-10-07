// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2017 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/syncoptions.{h,cpp}` (nextcloud/desktop v34.0.5).

//! `SyncOptions`: the options given to the sync engine. Virtual files are out
//! of scope, so the VFS is always "off".

use std::time::Duration;

use regex::Regex;

/// `SyncOptions::chunkV2MinChunkSize`: 5 MB.
pub const CHUNK_V2_MIN_CHUNK_SIZE: i64 = 5 * 1000 * 1000;
/// `SyncOptions::chunkV2MaxChunkSize`: 5 GB.
pub const CHUNK_V2_MAX_CHUNK_SIZE: i64 = 5 * 1000 * 1000 * 1000;

/// `SyncOptions`.
#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Maximum size (in bytes) a folder can have without asking for
    /// confirmation. -1 means infinite.
    pub new_big_folder_size_limit: i64,
    /// If a confirmation should be asked for external storages.
    pub confirm_external_storage: bool,
    /// Port addition: upstream reads `ConfigFile::notifyExistingFoldersOverLimit()`
    /// (a GUI setting, default false) from the discovery phase; the port
    /// carries it here.
    pub notify_existing_folders_over_limit: bool,
    /// If remotely deleted files are needed to move to trash.
    pub move_files_to_trash: bool,
    /// The initial un-adjusted chunk size in bytes for chunked uploads, which
    /// also classifies an item as "to be chunked".
    pub initial_chunk_size: i64,
    /// The target duration of chunk uploads for dynamic chunk sizing (0
    /// disables it).
    pub target_chunk_upload_duration: Duration,
    /// The maximum number of active jobs in parallel.
    pub parallel_network_jobs: i32,
    /// `SyncEngine::minimumFileAgeForUpload`: files modified more recently
    /// are not uploaded (2 s; `nextcloudcmd` and the tests use 0).
    pub minimum_file_age_for_upload: Duration,
    min_chunk_size: i64,
    max_chunk_size: i64,
    /// Only sync files that match the expression (`None` = upstream's
    /// invalid default regex, which matches nothing and disables the filter).
    file_regex: Option<Regex>,
    is_cmd: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            new_big_folder_size_limit: -1,
            confirm_external_storage: false,
            notify_existing_folders_over_limit: false,
            move_files_to_trash: false,
            initial_chunk_size: 100 * 1024 * 1024,
            target_chunk_upload_duration: Duration::from_secs(60),
            parallel_network_jobs: 6,
            minimum_file_age_for_upload: Duration::from_millis(2000),
            min_chunk_size: CHUNK_V2_MIN_CHUNK_SIZE,
            max_chunk_size: CHUNK_V2_MAX_CHUNK_SIZE,
            file_regex: None,
            is_cmd: false,
        }
    }
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        // QByteArray::toUInt(): 0 when invalid.
        .map(|v| v.trim().parse::<u32>().unwrap_or(0))
}

impl SyncOptions {
    pub fn min_chunk_size(&self) -> i64 {
        self.min_chunk_size
    }

    /// `setMinChunkSize`: bounded by the current min and max.
    pub fn set_min_chunk_size(&mut self, v: i64) {
        self.min_chunk_size = v.clamp(
            self.min_chunk_size,
            self.max_chunk_size.max(self.min_chunk_size),
        );
    }

    pub fn max_chunk_size(&self) -> i64 {
        self.max_chunk_size
    }

    /// `setMaxChunkSize`: bounded by the current min and max.
    pub fn set_max_chunk_size(&mut self, v: i64) {
        self.max_chunk_size = v.clamp(
            self.min_chunk_size,
            self.max_chunk_size.max(self.min_chunk_size),
        );
    }

    /// `fillFromEnvironmentVariables`: `OWNCLOUD_CHUNK_SIZE`,
    /// `OWNCLOUD_MIN_CHUNK_SIZE`, `OWNCLOUD_MAX_CHUNK_SIZE`,
    /// `OWNCLOUD_TARGET_CHUNK_UPLOAD_DURATION`, `OWNCLOUD_MAX_PARALLEL`.
    pub fn fill_from_environment_variables(&mut self) {
        if let Some(v) = env_u32("OWNCLOUD_CHUNK_SIZE") {
            self.initial_chunk_size = i64::from(v);
        }
        if let Some(v) = env_u32("OWNCLOUD_MIN_CHUNK_SIZE") {
            self.min_chunk_size = i64::from(v);
        }
        if let Some(v) = env_u32("OWNCLOUD_MAX_CHUNK_SIZE") {
            self.max_chunk_size = i64::from(v);
        }
        if let Some(v) = env_u32("OWNCLOUD_TARGET_CHUNK_UPLOAD_DURATION") {
            self.target_chunk_upload_duration = Duration::from_millis(u64::from(v));
        }
        let max_parallel = std::env::var("OWNCLOUD_MAX_PARALLEL")
            .ok()
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0);
        if max_parallel > 0 {
            self.parallel_network_jobs = max_parallel;
        }
    }

    /// `verifyChunkSizes`: ensure min <= initial <= max.
    pub fn verify_chunk_sizes(&mut self) {
        self.min_chunk_size = self.min_chunk_size.min(self.initial_chunk_size);
        self.max_chunk_size = self.max_chunk_size.max(self.initial_chunk_size);
    }

    /// `fileRegex()`.
    pub fn file_regex(&self) -> Option<&Regex> {
        self.file_regex.as_ref()
    }

    /// `setFilePattern`: a pattern like `*.txt` matching only file names.
    pub fn set_file_pattern(&mut self, pattern: &str) {
        // full match or a path ending with this pattern
        self.set_path_pattern(&format!("(^|/|\\\\){pattern}$"));
    }

    /// `setPathPattern`: a pattern matching the full path. An invalid
    /// pattern disables the filter, like an invalid `QRegularExpression`.
    pub fn set_path_pattern(&mut self, pattern: &str) {
        let pattern = if nc_journal::utility::fs_case_preserving() {
            format!("(?i){pattern}")
        } else {
            pattern.to_owned()
        };
        self.file_regex = Regex::new(&pattern).ok();
    }

    /// `isCmd()`: the sync was started by the command line client.
    pub fn is_cmd(&self) -> bool {
        self.is_cmd
    }

    pub fn set_is_cmd(&mut self, is_cmd: bool) {
        self.is_cmd = is_cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_chunk_size_bounds() {
        let mut o = SyncOptions {
            initial_chunk_size: 1000,
            ..SyncOptions::default()
        };
        o.verify_chunk_sizes();
        assert_eq!(o.min_chunk_size(), 1000);
        assert_eq!(o.max_chunk_size(), CHUNK_V2_MAX_CHUNK_SIZE);
        o.set_file_pattern(r".*\.txt");
        assert!(o.file_regex().unwrap().is_match("A/b.txt"));
        assert!(!o.file_regex().unwrap().is_match("A/b.txt2"));
    }
}
