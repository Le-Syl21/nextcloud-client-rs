// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/common/syncfilestatus.{h,cpp}` (nextcloud/desktop v34.0.5).

//! `SyncFileStatus`: the status of a file as shown by the shell integration
//! (overlay icons), with its socket API string.

/// `SyncFileStatus::SyncFileStatusTag`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SyncFileStatusTag {
    #[default]
    StatusNone,
    StatusSync,
    StatusWarning,
    StatusUpToDate,
    StatusError,
    StatusExcluded,
}

/// `SyncFileStatus`: a tag and the shared flag. Two statuses are equal when
/// both are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SyncFileStatus {
    tag: SyncFileStatusTag,
    shared: bool,
}

impl From<SyncFileStatusTag> for SyncFileStatus {
    fn from(tag: SyncFileStatusTag) -> Self {
        Self::new(tag)
    }
}

impl SyncFileStatus {
    /// `SyncFileStatus(tag)`.
    pub fn new(tag: SyncFileStatusTag) -> Self {
        Self { tag, shared: false }
    }

    /// `set(tag)`.
    pub fn set(&mut self, tag: SyncFileStatusTag) {
        self.tag = tag;
    }

    /// `tag()`.
    pub fn tag(&self) -> SyncFileStatusTag {
        self.tag
    }

    /// `setShared(isShared)`.
    pub fn set_shared(&mut self, is_shared: bool) {
        self.shared = is_shared;
    }

    /// `shared()`.
    pub fn shared(&self) -> bool {
        self.shared
    }

    /// `toSocketAPIString()`.
    pub fn to_socket_api_string(&self) -> String {
        let mut can_be_shared = true;
        let mut status_string = match self.tag {
            SyncFileStatusTag::StatusNone => {
                can_be_shared = false;
                "NOP"
            }
            SyncFileStatusTag::StatusSync => "SYNC",
            // The protocol says IGNORE, but all implementations show a yellow warning sign.
            SyncFileStatusTag::StatusWarning => "IGNORE",
            SyncFileStatusTag::StatusUpToDate => "OK",
            SyncFileStatusTag::StatusError => "ERROR",
            // The protocol says IGNORE, but all implementations show a yellow warning sign.
            SyncFileStatusTag::StatusExcluded => "IGNORE",
        }
        .to_owned();
        if can_be_shared && self.shared {
            status_string.push_str("+SWM");
        }
        status_string
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_socket_api_string() {
        use SyncFileStatusTag::*;
        let mut s = SyncFileStatus::new(StatusUpToDate);
        assert_eq!(s.to_socket_api_string(), "OK");
        s.set_shared(true);
        assert_eq!(s.to_socket_api_string(), "OK+SWM");
        s.set(StatusNone);
        assert_eq!(s.to_socket_api_string(), "NOP");
        s.set(StatusExcluded);
        assert_eq!(s.to_socket_api_string(), "IGNORE+SWM");
        assert_eq!(SyncFileStatus::default(), SyncFileStatus::new(StatusNone));
        assert_ne!(s, SyncFileStatus::new(StatusExcluded));
    }
}
