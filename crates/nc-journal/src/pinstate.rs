// SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2019 ownCloud GmbH
// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Port of upstream `src/common/pinstate.h` (nextcloud/desktop v34.0.5).

//! Pin states, stored in the journal's `flags` table.
//!
//! The pin state of a directory usually only matters for the initial pin and
//! hydration state of new remote files. Without virtual files (out of scope for
//! v1) the engine only reads them back; the values must still round-trip.

/// Determines whether items should be available locally permanently or not
/// (mimics `CF_PIN_STATE` of the Windows cfapi).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PinState {
    /// The pin state is derived from the state of the parent folder.
    ///
    /// The effective state for an item will never be `Inherited`.
    #[default]
    Inherited = 0,
    /// The file shall be available and up to date locally ("pinned").
    AlwaysLocal = 1,
    /// File shall be a dehydrated placeholder, filled on demand ("unpinned").
    OnlineOnly = 2,
    /// The user hasn't made a decision.
    Unspecified = 3,
    /// The file will never be synced to the cloud.
    Excluded = 4,
}

impl PinState {
    /// Converts the integer stored in the journal.
    ///
    /// Upstream `static_cast`s whatever is stored; unknown values map to
    /// `Inherited` here.
    pub fn from_db(value: i64) -> Self {
        match value {
            1 => Self::AlwaysLocal,
            2 => Self::OnlineOnly,
            3 => Self::Unspecified,
            4 => Self::Excluded,
            _ => Self::Inherited,
        }
    }
}

/// A user-facing version of [`PinState`]. The numerical values and ordering
/// are relevant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum VfsItemAvailability {
    /// The item and all its subitems are hydrated and pinned `AlwaysLocal`.
    AlwaysLocal = 0,
    /// The item and all its subitems are hydrated.
    AllHydrated = 1,
    /// There are dehydrated and hydrated items.
    Mixed = 2,
    /// There are only dehydrated items but the pin state isn't all `OnlineOnly`.
    AllDehydrated = 3,
    /// The item and all its subitems are dehydrated and `OnlineOnly`.
    OnlineOnly = 4,
}
