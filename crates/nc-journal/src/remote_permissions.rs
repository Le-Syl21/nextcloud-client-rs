/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2017 ownCloud GmbH
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

//! Memory-efficient storage of the remote permissions (`oc:permissions`).
//!
//! Port of upstream `src/common/remotepermissions.{h,cpp}` (v34.0.5).
//!
//! The value is a 16-bit set: bit 0 tells whether the value is set at all
//! (null vs. non-null), bit *n* is permission *n* of [`Permission`]. The
//! database encoding ([`RemotePermissions::to_db_value`]) is one letter per
//! permission; `""` is null and `" "` is non-null but empty.
//!
//! Quirks kept on purpose for bit-exactness:
//! - server strings are scanned per UTF-16 code unit truncated to 8 bits
//!   (`static_cast<char>`), so e.g. U+0147 counts as `'G'`;
//! - a code unit whose low byte is 0 (e.g. U+0100) matches the terminating
//!   NUL of upstream's letter table and sets bit 12, which is never written
//!   back by `to_db_value` but makes the value compare different;
//! - database values are read as C strings: scanning stops at the first NUL
//!   byte.

use std::collections::HashMap;
use std::fmt;

/// `" GWDNVCKRSMm"`: index = bit number. Index 12 is the C string terminator.
const LETTERS: &[u8; 12] = b" GWDNVCKRSMm";

const NOT_NULL_MASK: u16 = 0x1;

/// `RemotePermissions::Permissions`. The discriminant is the bit number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Permission {
    /// `G`
    CanRead = 1,
    /// `W`
    CanWrite,
    /// `D`
    CanDelete,
    /// `N`
    CanRename,
    /// `V`
    CanMove,
    /// `C`
    CanAddFile,
    /// `K`
    CanAddSubDirectories,
    /// `R`
    CanReshare,
    /// `S`. On the server this means SharedWithMe, but discovery also sets it
    /// when the server reports any `share-types`.
    IsShared,
    /// `M`
    IsMounted,
    /// `m` (internal: set if the parent dir has IsMounted)
    IsMountedSub,
}

impl Permission {
    /// `PermissionsCount` (== `IsMountedSub`).
    pub const COUNT: u8 = Permission::IsMountedSub as u8;
}

/// `RemotePermissions::MountedPermissionAlgorithm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MountedPermissionAlgorithm {
    UseMountRootProperty,
    #[default]
    WildGuessMountedSubProperty,
}

/// `OCC::RemotePermissions`. [`Default`] is the null value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RemotePermissions {
    value: u16,
}

impl RemotePermissions {
    /// Null permissions.
    pub const fn null() -> Self {
        Self { value: 0 }
    }

    fn from_units(units: impl Iterator<Item = u8>) -> Self {
        let mut value = NOT_NULL_MASK;
        for c in units {
            // std::strchr also finds the terminator when c == '\0'.
            let idx = if c == 0 {
                Some(LETTERS.len())
            } else {
                LETTERS.iter().position(|&l| l == c)
            };
            if let Some(idx) = idx {
                value |= 1 << idx;
            }
        }
        Self { value }
    }

    /// Array with one character per permission; empty is null, `" "` is
    /// non-null but empty.
    pub fn to_db_value(&self) -> Vec<u8> {
        if self.is_null() {
            return Vec::new();
        }
        let mut result = Vec::with_capacity(Permission::COUNT as usize);
        for (i, &letter) in LETTERS.iter().enumerate().skip(1) {
            if self.value & (1 << i) != 0 {
                result.push(letter);
            }
        }
        if result.is_empty() {
            // Make sure it is not empty so we can differentiate null and empty permissions
            result.push(b' ');
        }
        result
    }

    /// Reads a value that was written with [`to_db_value`](Self::to_db_value).
    pub fn from_db_value(value: &[u8]) -> Self {
        if value.is_empty() {
            return Self::null();
        }
        Self::from_units(value.iter().copied().take_while(|&c| c != 0))
    }

    /// Reads a permissions string received from the server; never null.
    /// Uses [`MountedPermissionAlgorithm::WildGuessMountedSubProperty`] and no
    /// other properties (upstream's default arguments).
    pub fn from_server_string(value: &str) -> Self {
        Self::from_server_string_with(
            value,
            MountedPermissionAlgorithm::default(),
            &HashMap::new(),
        )
    }

    /// Reads a permissions string received from the server, taking the other
    /// PROPFIND properties into account: `share-attributes` (a JSON array; an
    /// entry `{"scope":"permissions","key":"download","value":<not true>}`
    /// removes [`Permission::CanRead`]) and, with
    /// [`MountedPermissionAlgorithm::UseMountRootProperty`], `is-mount-root`
    /// (`M` on an entry that is not a mount root becomes `m`).
    pub fn from_server_string_with(
        value: &str,
        algorithm: MountedPermissionAlgorithm,
        other_properties: &HashMap<String, String>,
    ) -> Self {
        // QString::utf16() scanned until the first 0 unit, each unit cast to char.
        let mut perm = Self::from_units(
            value
                .encode_utf16()
                .take_while(|&u| u != 0)
                .map(|u| u as u8),
        );

        if let Some(raw) = other_properties.get("share-attributes") {
            decode_share_attributes(raw, &mut perm);
        }

        if algorithm == MountedPermissionAlgorithm::WildGuessMountedSubProperty {
            return perm;
        }

        let mount_root = other_properties.get("is-mount-root");
        if perm.has_permission(Permission::IsMounted) && mount_root.is_none_or(|v| v == "false") {
            // All the entries in an external storage have 'M' in their permission.
            // However, for all purposes in the desktop client, we only need to know
            // about the mount points. So replace the 'M' by a 'm' for every sub
            // entry in an external storage.
            perm.unset_permission(Permission::IsMounted);
            perm.set_permission(Permission::IsMountedSub);
        }
        perm
    }

    pub fn has_permission(&self, p: Permission) -> bool {
        self.value & (1 << p as u8) != 0
    }

    /// True if the local folder needs to be read/writeable for the current
    /// remote permissions set.
    pub fn has_permissions_for_read_write(&self) -> bool {
        [
            Permission::CanWrite,
            Permission::CanDelete,
            Permission::CanRename,
            Permission::CanMove,
            Permission::CanAddFile,
            Permission::CanAddSubDirectories,
        ]
        .into_iter()
        .any(|p| self.has_permission(p))
    }

    pub fn set_permission(&mut self, p: Permission) {
        self.value |= (1 << p as u8) | NOT_NULL_MASK;
    }

    pub fn unset_permission(&mut self, p: Permission) {
        self.value &= !(1 << p as u8);
    }

    pub fn is_null(&self) -> bool {
        self.value & NOT_NULL_MASK == 0
    }
}

fn decode_share_attributes(raw: &str, perm: &mut RemotePermissions) {
    // QJsonDocument::fromJson + .array(): anything but a JSON array is empty.
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(raw)
    else {
        return;
    };
    let mut missing_download_permission = false;
    for entry in &entries {
        // QJsonValue::toObject() on a non-object is an empty object.
        let Some(obj) = entry.as_object() else {
            continue;
        };
        if obj.get("scope").and_then(|v| v.as_str()) == Some("permissions")
            && obj.get("key").and_then(|v| v.as_str()) == Some("download")
        {
            // QJsonValue::toBool() is false for anything but `true`.
            if obj
                .get("value")
                .is_some_and(|v| !v.as_bool().unwrap_or(false))
            {
                missing_download_permission = true;
            }
            break;
        }
    }
    if missing_download_permission {
        perm.unset_permission(Permission::CanRead);
    }
}

/// Display form, same as upstream `toString()` (== the database value).
impl fmt::Display for RemotePermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.to_db_value()))
    }
}

#[cfg(test)]
mod tests {
    //! Derived tests: upstream has no dedicated test file; these follow
    //! `remotepermissions.{h,cpp}` and the strings used in
    //! `test/testpermissions.cpp`.
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn null_and_empty_db_values() {
        let null = RemotePermissions::default();
        assert!(null.is_null());
        assert_eq!(null.to_db_value(), b"");
        assert_eq!(RemotePermissions::from_db_value(b""), null);

        let empty = RemotePermissions::from_server_string("");
        assert!(!empty.is_null());
        assert_eq!(empty.to_db_value(), b" ");
        assert_eq!(RemotePermissions::from_db_value(b" "), empty);
        assert_ne!(empty, null);
    }

    #[test]
    fn letters_round_trip_in_canonical_order() {
        let p = RemotePermissions::from_server_string("mMSRKCVNDWG");
        assert_eq!(p.to_db_value(), b"GWDNVCKRSMm");
        assert_eq!(p.to_string(), "GWDNVCKRSMm");
        assert_eq!(RemotePermissions::from_db_value(&p.to_db_value()), p);
        for (i, perm) in [
            Permission::CanRead,
            Permission::CanWrite,
            Permission::CanDelete,
            Permission::CanRename,
            Permission::CanMove,
            Permission::CanAddFile,
            Permission::CanAddSubDirectories,
            Permission::CanReshare,
            Permission::IsShared,
            Permission::IsMounted,
            Permission::IsMountedSub,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(perm as usize, i + 1);
            assert!(p.has_permission(perm));
        }
    }

    #[test]
    fn testpermissions_strings() {
        // Strings used by upstream test/testpermissions.cpp.
        let norename = RemotePermissions::from_server_string("GWDVCK");
        assert!(!norename.has_permission(Permission::CanRename));
        assert!(norename.has_permission(Permission::CanMove));
        let nomove = RemotePermissions::from_server_string("GWDNCK");
        assert!(!nomove.has_permission(Permission::CanMove));
        let nocreatefile = RemotePermissions::from_server_string("GWDNVK");
        assert!(!nocreatefile.has_permission(Permission::CanAddFile));
        let nocreatedir = RemotePermissions::from_server_string("GWDNVC");
        assert!(!nocreatedir.has_permission(Permission::CanAddSubDirectories));
        let readonly = RemotePermissions::from_server_string("GWDNV");
        assert_eq!(readonly.to_db_value(), b"GWDNV");
        let forbidden_move = RemotePermissions::from_server_string("WNCKG");
        assert_eq!(forbidden_move.to_db_value(), b"GWNCK");
        let ro_folder = RemotePermissions::from_server_string("MG");
        assert!(!ro_folder.has_permissions_for_read_write());
        assert!(ro_folder.has_permission(Permission::IsMounted)); // wild guess keeps M
        let rw_folder = RemotePermissions::from_server_string("GCKWDNVRSM");
        assert!(rw_folder.has_permissions_for_read_write());
    }

    #[test]
    fn unknown_letters_ignored_and_set_unset() {
        let mut p = RemotePermissions::from_server_string("GXZ?");
        assert_eq!(p.to_db_value(), b"G");
        p.unset_permission(Permission::CanRead);
        assert!(!p.is_null());
        assert_eq!(p.to_db_value(), b" ");
        let mut q = RemotePermissions::null();
        q.set_permission(Permission::CanWrite);
        assert!(!q.is_null());
        assert_eq!(q.to_db_value(), b"W");
    }

    #[test]
    fn share_attributes_download_forbidden() {
        let forbidden = props(&[(
            "share-attributes",
            r#"[{"scope":"permissions","key":"download","value":false}]"#,
        )]);
        let allowed = props(&[(
            "share-attributes",
            r#"[{"scope":"permissions","key":"download","value":true}]"#,
        )]);
        let algo = MountedPermissionAlgorithm::WildGuessMountedSubProperty;
        assert!(
            !RemotePermissions::from_server_string_with("GSRDNVCKW", algo, &forbidden)
                .has_permission(Permission::CanRead)
        );
        assert!(
            RemotePermissions::from_server_string_with("GSRDNVCKW", algo, &allowed)
                .has_permission(Permission::CanRead)
        );
        // toBool() of a non-bool is false
        let string_value = props(&[(
            "share-attributes",
            r#"[{"scope":"permissions","key":"download","value":"true"}]"#,
        )]);
        assert!(
            !RemotePermissions::from_server_string_with("G", algo, &string_value)
                .has_permission(Permission::CanRead)
        );
        // invalid JSON / object document: ignored
        let invalid = props(&[("share-attributes", "{not json")]);
        assert!(
            RemotePermissions::from_server_string_with("G", algo, &invalid)
                .has_permission(Permission::CanRead)
        );
    }

    #[test]
    fn mount_root_algorithm() {
        let use_root = MountedPermissionAlgorithm::UseMountRootProperty;
        let sub = RemotePermissions::from_server_string_with(
            "GM",
            use_root,
            &props(&[("is-mount-root", "false")]),
        );
        assert!(!sub.has_permission(Permission::IsMounted));
        assert!(sub.has_permission(Permission::IsMountedSub));
        let missing = RemotePermissions::from_server_string_with("GM", use_root, &HashMap::new());
        assert!(missing.has_permission(Permission::IsMountedSub));
        let root = RemotePermissions::from_server_string_with(
            "GM",
            use_root,
            &props(&[("is-mount-root", "true")]),
        );
        assert!(root.has_permission(Permission::IsMounted));
        assert!(!root.has_permission(Permission::IsMountedSub));
    }

    #[test]
    fn utf16_truncation_quirks() {
        // U+0147 truncates to 'G'
        assert!(
            RemotePermissions::from_server_string("\u{147}").has_permission(Permission::CanRead)
        );
        // U+0100 truncates to NUL: bit 12, invisible in the db value
        let odd = RemotePermissions::from_server_string("\u{100}");
        assert_eq!(odd.to_db_value(), b" ");
        assert_ne!(odd, RemotePermissions::from_server_string(""));
        // db values stop at the first NUL byte
        assert_eq!(
            RemotePermissions::from_db_value(b"G\0W").to_db_value(),
            b"G"
        );
    }
}
