// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the parts of upstream `src/libsync/configfile.cpp` the daemon needs
// (nextcloud/desktop v34.0.5).

//! `ConfigFile`: the client-wide settings of `nextcloud.cfg` (same keys,
//! groups and defaults), plus where the daemon's own file lives.
//!
//! Keys without group are in `[General]`; the bandwidth keys are in
//! `[BWLimit]`; the polling intervals are read from the "connection group",
//! `Theme::appName()`, i.e. `[Nextcloud]` for the unbranded client (kept as
//! is, so the files stay interchangeable).
//!
//! Not ported: the system-wide defaults upstream's `getValue()` reads from
//! `/etc/Nextcloud/Nextcloud.conf` (they belong to the official client) and
//! the Windows policy registry keys; a key missing from our file gets the
//! built-in default.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::settings::Settings;

/// `Theme::appName()` of the unbranded client: the connection group, and
/// the QtKeychain service name of the official client.
pub const APP_NAME: &str = "Nextcloud";

const DEFAULT_REMOTE_POLL_INTERVAL_MS: i64 = 30_000;
/// `DEFAULT_MAX_LOG_LINES`.
pub const DEFAULT_MAX_LOG_LINES: i64 = 20_000;
const DELETE_FILES_THRESHOLD_DEFAULT: i64 = 100;
/// `Theme::newBigFolderSizeLimit()` (MB).
const DEFAULT_NEW_BIG_FOLDER_SIZE_LIMIT_MB: i64 = 500;

const REMOTE_POLL_INTERVAL: &str = "remotePollInterval";
const FORCE_SYNC_INTERVAL: &str = "forceSyncInterval";
const FULL_LOCAL_DISCOVERY_INTERVAL: &str = "fullLocalDiscoveryInterval";
const NOTIFICATION_REFRESH_INTERVAL: &str = "notificationRefreshInterval";
const DELETE_FILES_THRESHOLD: &str = "deleteFilesThreshold";
const TIMEOUT: &str = "timeout";
const CHUNK_SIZE: &str = "chunkSize";
const MIN_CHUNK_SIZE: &str = "minChunkSize";
const MAX_CHUNK_SIZE: &str = "maxChunkSize";
const TARGET_CHUNK_UPLOAD_DURATION: &str = "targetChunkUploadDuration";
const PROMPT_DELETE: &str = "promptDeleteAllFiles";
const CONFIRM_EXTERNAL_STORAGE: &str = "confirmExternalStorage";
const USE_NEW_BIG_FOLDER_SIZE_LIMIT: &str = "useNewBigFolderSizeLimit";
const NEW_BIG_FOLDER_SIZE_LIMIT: &str = "newBigFolderSizeLimit";
const NOTIFY_EXISTING_FOLDERS_OVER_LIMIT: &str = "notifyExistingFoldersOverLimit";
const STOP_SYNCING_EXISTING_FOLDERS_OVER_LIMIT: &str = "stopSyncingExistingFoldersOverLimit";
const MOVE_TO_TRASH: &str = "moveToTrash";
const USE_UPLOAD_LIMIT: &str = "BWLimit/useUploadLimit";
const USE_DOWNLOAD_LIMIT: &str = "BWLimit/useDownloadLimit";
const UPLOAD_LIMIT: &str = "BWLimit/uploadLimit";
const DOWNLOAD_LIMIT: &str = "BWLimit/downloadLimit";
const CLIENT_VERSION: &str = "clientVersion";

/// The client-wide settings (`ConfigFile`) over a loaded settings document.
#[derive(Clone, Copy, Debug)]
pub struct ConfigFile<'a> {
    settings: &'a Settings,
}

fn ms(settings: &Settings, key: &str, default: Duration) -> i64 {
    settings.i64_or(key, default.as_millis() as i64)
}

fn duration_ms(value: i64) -> Duration {
    Duration::from_millis(value.max(0) as u64)
}

impl<'a> ConfigFile<'a> {
    pub fn new(settings: &'a Settings) -> Self {
        Self { settings }
    }

    fn connection_key(key: &str) -> String {
        format!("{APP_NAME}/{key}")
    }

    /// `remotePollInterval()`: 30 s by default, back to the default below 5 s.
    pub fn remote_poll_interval(&self) -> Duration {
        let default = DEFAULT_REMOTE_POLL_INTERVAL_MS;
        let v = self
            .settings
            .i64_or(&Self::connection_key(REMOTE_POLL_INTERVAL), default);
        if v < 5_000 {
            if self
                .settings
                .contains(&Self::connection_key(REMOTE_POLL_INTERVAL))
            {
                log::warn!(target: "nextcloud.sync.configfile", "Remote Interval is less than 5 seconds, reverting to {default}");
            }
            return duration_ms(default);
        }
        duration_ms(v)
    }

    /// `forceSyncInterval()`: 2 h by default, at least the poll interval.
    pub fn force_sync_interval(&self) -> Duration {
        let poll = self.remote_poll_interval();
        let v = ms(
            self.settings,
            &Self::connection_key(FORCE_SYNC_INTERVAL),
            Duration::from_secs(2 * 3600),
        );
        let interval = duration_ms(v);
        if v < 0 || interval < poll {
            log::warn!(target: "nextcloud.sync.configfile", "Force sync interval is less than the remote poll interval, reverting to {}", poll.as_millis());
            return poll;
        }
        interval
    }

    /// `fullLocalDiscoveryInterval()` in milliseconds (1 h by default); a
    /// negative value means "no periodic full discovery".
    pub fn full_local_discovery_interval_ms(&self) -> i64 {
        ms(
            self.settings,
            &Self::connection_key(FULL_LOCAL_DISCOVERY_INTERVAL),
            Duration::from_secs(3600),
        )
    }

    /// The interval `Folder::startSync` uses: the setting, overridden by
    /// `OWNCLOUD_FULL_LOCAL_DISCOVERY_INTERVAL` (milliseconds) when set.
    /// `None` when periodic full discoveries are disabled (negative).
    pub fn full_local_discovery_interval_with_env(&self) -> Option<Duration> {
        let mut v = self.full_local_discovery_interval_ms();
        if let Ok(env) = std::env::var("OWNCLOUD_FULL_LOCAL_DISCOVERY_INTERVAL")
            && !env.is_empty()
        {
            // QByteArray::toLongLong: 0 when not a number.
            v = env.trim().parse::<i64>().unwrap_or(0);
        }
        (v >= 0).then(|| duration_ms(v))
    }

    /// `notificationRefreshInterval()`: 1 min by default and at least.
    pub fn notification_refresh_interval(&self) -> Duration {
        let v = ms(
            self.settings,
            &Self::connection_key(NOTIFICATION_REFRESH_INTERVAL),
            Duration::from_secs(60),
        );
        duration_ms(v.max(60_000))
    }

    /// `timeout()` in seconds (300).
    pub fn timeout(&self) -> i64 {
        self.settings.i64_or(TIMEOUT, 300)
    }

    /// `chunkSize()` (100 MiB).
    pub fn chunk_size(&self) -> i64 {
        self.settings.i64_or(CHUNK_SIZE, 100 * 1024 * 1024)
    }

    /// `maxChunkSize()` (100 MiB).
    pub fn max_chunk_size(&self) -> i64 {
        self.settings.i64_or(MAX_CHUNK_SIZE, 100 * 1024 * 1024)
    }

    /// `minChunkSize()` (5 MiB).
    pub fn min_chunk_size(&self) -> i64 {
        self.settings.i64_or(MIN_CHUNK_SIZE, 5 * 1024 * 1024)
    }

    /// `targetChunkUploadDuration()` (1 min).
    pub fn target_chunk_upload_duration(&self) -> Duration {
        duration_ms(ms(
            self.settings,
            TARGET_CHUNK_UPLOAD_DURATION,
            Duration::from_secs(60),
        ))
    }

    /// `promptDeleteFiles()` (false).
    pub fn prompt_delete_files(&self) -> bool {
        self.settings.bool_or(PROMPT_DELETE, false)
    }

    /// `deleteFilesThreshold()` (100).
    pub fn delete_files_threshold(&self) -> i64 {
        self.settings
            .i64_or(DELETE_FILES_THRESHOLD, DELETE_FILES_THRESHOLD_DEFAULT)
    }

    /// `useUploadLimit()`: 0 none, 1 manual, -1 automatic (removed upstream).
    pub fn use_upload_limit(&self) -> i64 {
        self.settings.i64_or(USE_UPLOAD_LIMIT, 0)
    }

    /// `useDownloadLimit()`.
    pub fn use_download_limit(&self) -> i64 {
        self.settings.i64_or(USE_DOWNLOAD_LIMIT, 0)
    }

    /// `uploadLimit()` in KB/s (10).
    pub fn upload_limit(&self) -> i64 {
        self.settings.i64_or(UPLOAD_LIMIT, 10)
    }

    /// `downloadLimit()` in KB/s (80).
    pub fn download_limit(&self) -> i64 {
        self.settings.i64_or(DOWNLOAD_LIMIT, 80)
    }

    /// `newBigFolderSizeLimit()`: (enabled, limit in MB). Enabled when the
    /// limit is not negative and `useNewBigFolderSizeLimit` is set.
    pub fn new_big_folder_size_limit(&self) -> (bool, i64) {
        let value = self.settings.i64_or(
            NEW_BIG_FOLDER_SIZE_LIMIT,
            DEFAULT_NEW_BIG_FOLDER_SIZE_LIMIT_MB,
        );
        let enabled = value >= 0 && self.use_new_big_folder_size_limit();
        (enabled, value.max(0))
    }

    /// `useNewBigFolderSizeLimit()` (true).
    pub fn use_new_big_folder_size_limit(&self) -> bool {
        self.settings.bool_or(USE_NEW_BIG_FOLDER_SIZE_LIMIT, true)
    }

    /// `confirmExternalStorage()` (true).
    pub fn confirm_external_storage(&self) -> bool {
        self.settings.bool_or(CONFIRM_EXTERNAL_STORAGE, true)
    }

    /// `notifyExistingFoldersOverLimit()` (false).
    pub fn notify_existing_folders_over_limit(&self) -> bool {
        self.settings
            .bool_or(NOTIFY_EXISTING_FOLDERS_OVER_LIMIT, false)
    }

    /// `stopSyncingExistingFoldersOverLimit()` (defaults to the notify
    /// setting).
    pub fn stop_syncing_existing_folders_over_limit(&self) -> bool {
        let fallback = self.notify_existing_folders_over_limit();
        self.settings
            .bool_or(STOP_SYNCING_EXISTING_FOLDERS_OVER_LIMIT, fallback)
    }

    /// `moveToTrash()` (false).
    pub fn move_to_trash(&self) -> bool {
        self.settings.bool_or(MOVE_TO_TRASH, false)
    }

    /// `clientVersionString()` as stored (empty when never written).
    pub fn client_version(&self) -> String {
        self.settings.string(CLIENT_VERSION)
    }
}

/// How the daemon runs: as a user service (credentials in the keyring) or
/// as a system instance `ncsyncd@USER` running as USER (credentials from
/// systemd or a password file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceMode {
    User,
    System { instance: String },
}

/// Errors about configuration locations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LocationError {
    #[error("invalid instance name {0:?} (use letters, digits, '-', '_', '.' and no leading '.')")]
    InvalidInstance(String),
    #[error("cannot find the home directory ($HOME is not set)")]
    NoHome,
}

/// Where the configuration file and the state live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigLocation {
    pub mode: ServiceMode,
    /// The `nextcloud.cfg`-format configuration file.
    pub config_file: PathBuf,
    /// Daemon state (sockets, password files written by `account add` for a
    /// system instance, ...).
    pub state_dir: PathBuf,
}

fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf, LocationError> {
    if let Some(v) = std::env::var_os(var)
        && Path::new(&v).is_absolute()
    {
        return Ok(PathBuf::from(v));
    }
    let home = std::env::var_os("HOME").ok_or(LocationError::NoHome)?;
    Ok(PathBuf::from(home).join(fallback))
}

impl ConfigLocation {
    /// The user daemon: `$XDG_CONFIG_HOME/ncsyncd/ncsyncd.cfg` (default
    /// `~/.config/ncsyncd/ncsyncd.cfg`), state in `$XDG_STATE_HOME/ncsyncd`
    /// (default `~/.local/state/ncsyncd`).
    pub fn user() -> Result<Self, LocationError> {
        Ok(Self {
            mode: ServiceMode::User,
            config_file: xdg_dir("XDG_CONFIG_HOME", ".config")?.join("ncsyncd/ncsyncd.cfg"),
            state_dir: xdg_dir("XDG_STATE_HOME", ".local/state")?.join("ncsyncd"),
        })
    }

    /// The system instance `ncsyncd@USER`, which runs as the user USER
    /// (`User=%i`): state in `/var/lib/ncsyncd/USER` (the unit's
    /// `StateDirectory=`, owned by USER), configuration
    /// `/var/lib/ncsyncd/USER/ncsyncd.cfg` there too, since the daemon
    /// rewrites it (pause state, migrations) and needs a writable directory
    /// for its lock file and atomic writes.
    pub fn system(instance: &str) -> Result<Self, LocationError> {
        let valid = !instance.is_empty()
            && !instance.starts_with('.')
            && instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !valid {
            return Err(LocationError::InvalidInstance(instance.to_owned()));
        }
        Ok(Self {
            mode: ServiceMode::System {
                instance: instance.to_owned(),
            },
            config_file: PathBuf::from(format!("/var/lib/ncsyncd/{instance}/ncsyncd.cfg")),
            state_dir: PathBuf::from(format!("/var/lib/ncsyncd/{instance}")),
        })
    }

    /// `--config PATH`: another configuration file, same mode and state.
    pub fn with_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = path.into();
        self
    }
}

/// The official client's configuration: `$XDG_CONFIG_HOME/Nextcloud/nextcloud.cfg`
/// (`QStandardPaths::AppConfigLocation` + `Theme::configFileName()`).
pub fn official_client_config_file() -> Result<PathBuf, LocationError> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("Nextcloud/nextcloud.cfg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_defaults_on_empty_settings() {
        let s = Settings::new();
        let c = ConfigFile::new(&s);
        assert_eq!(c.remote_poll_interval(), Duration::from_secs(30));
        assert_eq!(c.force_sync_interval(), Duration::from_secs(7200));
        assert_eq!(c.full_local_discovery_interval_ms(), 3_600_000);
        assert_eq!(c.notification_refresh_interval(), Duration::from_secs(60));
        assert_eq!(c.timeout(), 300);
        assert_eq!(c.chunk_size(), 100 * 1024 * 1024);
        assert_eq!(c.max_chunk_size(), 100 * 1024 * 1024);
        assert_eq!(c.min_chunk_size(), 5 * 1024 * 1024);
        assert_eq!(c.target_chunk_upload_duration(), Duration::from_secs(60));
        assert!(!c.prompt_delete_files());
        assert_eq!(c.delete_files_threshold(), 100);
        assert_eq!(
            (
                c.use_upload_limit(),
                c.upload_limit(),
                c.use_download_limit(),
                c.download_limit()
            ),
            (0, 10, 0, 80)
        );
        assert_eq!(c.new_big_folder_size_limit(), (true, 500));
        assert!(c.confirm_external_storage());
        assert!(!c.move_to_trash());
        assert!(!c.stop_syncing_existing_folders_over_limit());
    }

    #[test]
    fn derived_values_and_clamps() {
        let s = Settings::parse(
            "[General]\nnewBigFolderSizeLimit=-1\nmoveToTrash=true\nconfirmExternalStorage=false\n\
             notifyExistingFoldersOverLimit=true\n\n[BWLimit]\nuseUploadLimit=1\nuploadLimit=250\n\n\
             [Nextcloud]\nremotePollInterval=1000\nforceSyncInterval=10\nnotificationRefreshInterval=5\n\
             fullLocalDiscoveryInterval=-1\n",
        )
        .unwrap();
        let c = ConfigFile::new(&s);
        assert_eq!(c.remote_poll_interval(), Duration::from_secs(30));
        assert_eq!(c.force_sync_interval(), Duration::from_secs(30));
        assert_eq!(c.notification_refresh_interval(), Duration::from_secs(60));
        assert_eq!(c.full_local_discovery_interval_ms(), -1);
        assert_eq!(c.new_big_folder_size_limit(), (false, 0));
        assert!(c.move_to_trash());
        assert!(!c.confirm_external_storage());
        assert!(c.stop_syncing_existing_folders_over_limit());
        assert_eq!((c.use_upload_limit(), c.upload_limit()), (1, 250));

        let s = Settings::parse("[Nextcloud]\nremotePollInterval=60000\n").unwrap();
        assert_eq!(
            ConfigFile::new(&s).remote_poll_interval(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn derived_locations() {
        let sys = ConfigLocation::system("work").unwrap();
        assert_eq!(sys.config_file, PathBuf::from("/var/lib/ncsyncd/work/ncsyncd.cfg"));
        assert_eq!(sys.state_dir, PathBuf::from("/var/lib/ncsyncd/work"));
        assert!(ConfigLocation::system("../x").is_err());
        assert!(ConfigLocation::system("").is_err());
        let o = sys.clone().with_config_file("/tmp/x.cfg");
        assert_eq!(o.config_file, PathBuf::from("/tmp/x.cfg"));
        assert_eq!(o.mode, sys.mode);
    }
}
