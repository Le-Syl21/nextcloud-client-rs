// libcsync -- a library to sync a directory with another
//
// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2012 ownCloud GmbH
// SPDX-FileCopyrightText: 2008-2013 Andreas Schneider <asn@cryptomilk.org>
// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Port of the enums of upstream `src/csync/csync.h` (nextcloud/desktop v34.0.5).

//! Core csync enums shared by the journal, the exclude engine and the sync engine.

/// Type of a sync item (`ItemType`).
///
/// This value is stored in the journal's `metadata.type` column, so the
/// discriminants must never change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ItemType {
    File = 0,
    SoftLink = 1,
    Directory = 2,
    #[default]
    Skip = 3,
    /// The file is a dehydrated placeholder, meaning data isn't available locally.
    VirtualFile = 4,
    /// A `VirtualFile` that wants to be hydrated.
    VirtualFileDownload = 5,
    /// A `File` that wants to be dehydrated.
    VirtualFileDehydration = 6,
    VirtualDirectory = 7,
}

impl ItemType {
    /// Converts the integer stored in the journal.
    ///
    /// Upstream does a `static_cast<ItemType>` on whatever is stored; unknown
    /// values have no meaning there, we map them to [`ItemType::Skip`].
    pub fn from_db(value: i64) -> Self {
        match value {
            0 => Self::File,
            1 => Self::SoftLink,
            2 => Self::Directory,
            4 => Self::VirtualFile,
            5 => Self::VirtualFileDownload,
            6 => Self::VirtualFileDehydration,
            7 => Self::VirtualDirectory,
            _ => Self::Skip,
        }
    }
}

/// Encryption status of an item as seen by the sync engine (`ItemEncryptionStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ItemEncryptionStatus {
    #[default]
    NotEncrypted = 0,
    Encrypted = 1,
    EncryptedMigratedV1_2 = 2,
    EncryptedMigratedV2_0 = 3,
}

/// Encryption status as stored in the journal's `isE2eEncrypted` column
/// (`JournalDbEncryptionStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum JournalDbEncryptionStatus {
    #[default]
    NotEncrypted = 0,
    Encrypted = 1,
    EncryptedMigratedV1_2Invalid = 2,
    EncryptedMigratedV1_2 = 3,
    EncryptedMigratedV2_0 = 4,
}

impl JournalDbEncryptionStatus {
    /// Converts the integer stored in the journal (unknown values map to `NotEncrypted`).
    pub fn from_db(value: i64) -> Self {
        match value {
            1 => Self::Encrypted,
            2 => Self::EncryptedMigratedV1_2Invalid,
            3 => Self::EncryptedMigratedV1_2,
            4 => Self::EncryptedMigratedV2_0,
            _ => Self::NotEncrypted,
        }
    }
}
