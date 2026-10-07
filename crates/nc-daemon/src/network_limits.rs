// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the per-account network transfer limits of upstream
// `src/libsync/account.{h,cpp}` (`AccountNetworkTransferLimitSetting`,
// `set{Upload,Download}LimitSetting`, `{upload,download}Limit`) and of
// `Folder::setDirtyNetworkLimits` (`src/gui/folder.cpp`)
// (nextcloud/desktop v34.0.5).

//! The bandwidth limits of an account, as the folders apply them to their
//! sync engine.

/// `Account::AccountNetworkTransferLimitSetting`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LimitSetting {
    /// -2: the pre-3.14 global setting (no longer supported).
    LegacyGlobalLimit,
    /// -1: the deprecated automatic limit.
    AutoLimit,
    /// 0
    #[default]
    NoLimit,
    /// 1: `uploadLimit` / `downloadLimit` KB/s.
    ManualLimit,
}

impl LimitSetting {
    /// The integer stored in the settings.
    pub fn to_int(self) -> i32 {
        match self {
            Self::LegacyGlobalLimit => -2,
            Self::AutoLimit => -1,
            Self::NoLimit => 0,
            Self::ManualLimit => 1,
        }
    }

    /// From the integer stored in the settings (unknown values: no limit).
    pub fn from_int(v: i64) -> Self {
        match v {
            -2 => Self::LegacyGlobalLimit,
            -1 => Self::AutoLimit,
            1 => Self::ManualLimit,
            _ => Self::NoLimit,
        }
    }
}

/// The limit settings of one account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkLimits {
    upload_limit_setting: LimitSetting,
    download_limit_setting: LimitSetting,
    /// KB/s
    upload_limit: u32,
    /// KB/s
    download_limit: u32,
}

/// `setUploadLimitSetting` / `setDownloadLimitSetting`: the legacy global
/// and the automatic limits fall back to unlimited.
fn fallback(setting: LimitSetting, what: &str) -> LimitSetting {
    match setting {
        LimitSetting::LegacyGlobalLimit => {
            log::info!(target: "nextcloud.sync.account", "{what} limit setting was requested to be set to the legacy global limit, falling back to unlimited");
            LimitSetting::NoLimit
        }
        LimitSetting::AutoLimit => {
            log::info!(target: "nextcloud.sync.account", "{what} limit setting was requested to be set to the deprecated auto limit, falling back to unlimited");
            LimitSetting::NoLimit
        }
        s => s,
    }
}

impl NetworkLimits {
    pub fn upload_limit_setting(&self) -> LimitSetting {
        self.upload_limit_setting
    }

    pub fn download_limit_setting(&self) -> LimitSetting {
        self.download_limit_setting
    }

    pub fn set_upload_limit_setting(&mut self, setting: LimitSetting) {
        if setting != self.upload_limit_setting {
            self.upload_limit_setting = fallback(setting, "Upload");
        }
    }

    pub fn set_download_limit_setting(&mut self, setting: LimitSetting) {
        if setting != self.download_limit_setting {
            self.download_limit_setting = fallback(setting, "Download");
        }
    }

    pub fn upload_limit(&self) -> u32 {
        self.upload_limit
    }

    pub fn download_limit(&self) -> u32 {
        self.download_limit
    }

    pub fn set_upload_limit(&mut self, kbps: u32) {
        self.upload_limit = kbps;
    }

    pub fn set_download_limit(&mut self, kbps: u32) {
        self.download_limit = kbps;
    }

    /// `Folder::setDirtyNetworkLimits()`: the `(upload, download)` values
    /// given to `SyncEngine::setNetworkLimits`, in bytes per second (0 = no
    /// limit).
    pub fn engine_limits(&self) -> (i32, i32) {
        let to_engine = |setting: LimitSetting, limit: u32| -> i32 {
            if setting.to_int() >= 1 {
                (i64::from(limit) * 1000).min(i64::from(i32::MAX)) as i32
            } else {
                0
            }
        };
        (
            to_engine(self.upload_limit_setting, self.upload_limit),
            to_engine(self.download_limit_setting, self.download_limit),
        )
    }
}

// Port of upstream test/testaccount.cpp
// testAccount_setLimitSettings_globalNetworkLimitFallback (CC0-1.0,
// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors,
// 2016 ownCloud GmbH).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_set_limit_settings_global_network_limit_fallback() {
        let mut account = NetworkLimits::default();
        let set_limit_settings = |a: &mut NetworkLimits, setting: LimitSetting| {
            a.set_download_limit_setting(setting);
            a.set_upload_limit_setting(setting);
        };
        let verify_limit_settings = |a: &NetworkLimits, expected: LimitSetting| {
            assert_eq!(expected, a.download_limit_setting());
            assert_eq!(expected, a.upload_limit_setting());
        };
        // the default setting should be NoLimit
        verify_limit_settings(&account, LimitSetting::NoLimit);
        // changing it to ManualLimit should succeed
        set_limit_settings(&mut account, LimitSetting::ManualLimit);
        verify_limit_settings(&account, LimitSetting::ManualLimit);
        // changing it to AutoLimit should fall back to NoLimit
        set_limit_settings(&mut account, LimitSetting::AutoLimit);
        verify_limit_settings(&account, LimitSetting::NoLimit);
        // changing it to LegacyGlobalLimit (-2) should fall back to NoLimit
        set_limit_settings(&mut account, LimitSetting::LegacyGlobalLimit);
        verify_limit_settings(&account, LimitSetting::NoLimit);
    }

    #[test]
    fn derived_engine_limits_are_bytes_per_second() {
        let mut a = NetworkLimits::default();
        a.set_upload_limit(100);
        a.set_download_limit(50);
        assert_eq!(a.engine_limits(), (0, 0));
        a.set_upload_limit_setting(LimitSetting::ManualLimit);
        assert_eq!(a.engine_limits(), (100_000, 0));
    }
}
