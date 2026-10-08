// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/clientstatusreportingcommon.{h,cpp}`
// (nextcloud/desktop v34.0.5).

//! The statuses the sync engine reports to the server's `security_guard`
//! diagnostics (see `nc_sync::client_status_reporting`).

/// `ClientStatusReportingStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ClientStatusReportingStatus {
    DownloadErrorConflictCaseClash = 0,
    DownloadErrorConflictInvalidCharacters,
    DownloadErrorServerError,
    DownloadErrorVirtualFileHydrationFailure,
    E2EeErrorGeneralError,
    UploadErrorServerError,
    UploadErrorVirusDetected,
}

impl ClientStatusReportingStatus {
    /// `ClientStatusReportingStatus::Count`.
    pub const COUNT: i32 = 7;

    /// Every status, in number order.
    pub const ALL: [Self; Self::COUNT as usize] = [
        Self::DownloadErrorConflictCaseClash,
        Self::DownloadErrorConflictInvalidCharacters,
        Self::DownloadErrorServerError,
        Self::DownloadErrorVirtualFileHydrationFailure,
        Self::E2EeErrorGeneralError,
        Self::UploadErrorServerError,
        Self::UploadErrorVirusDetected,
    ];

    /// The status of a number, `None` outside `0..Count`.
    pub fn from_number(n: i64) -> Option<Self> {
        usize::try_from(n)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
    }

    /// `clientStatusstatusStringFromNumber(status)`.
    pub fn status_string(self) -> &'static str {
        match self {
            Self::DownloadErrorConflictCaseClash => "DownloadError.CONFLICT_CASECLASH",
            Self::DownloadErrorConflictInvalidCharacters => {
                "DownloadError.CONFLICT_INVALID_CHARACTERS"
            }
            Self::DownloadErrorServerError => "DownloadError.SERVER_ERROR",
            Self::DownloadErrorVirtualFileHydrationFailure => {
                "DownloadError.VIRTUAL_FILE_HYDRATION_FAILURE"
            }
            Self::E2EeErrorGeneralError => "E2EeError.General",
            Self::UploadErrorServerError => "UploadError.SERVER_ERROR",
            Self::UploadErrorVirusDetected => "UploadResult.VIRUS_DETECTED",
        }
    }
}

/// Where an account's reported statuses go (`ClientStatusReporting`).
pub trait ClientStatusReporter: Send + Sync {
    /// `reportClientStatus(status)`.
    fn report_client_status(&self, status: ClientStatusReportingStatus);
}
