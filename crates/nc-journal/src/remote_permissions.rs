// SPDX-License-Identifier: LGPL-2.1-or-later
//! TEMPORARY stub (replaced by the full port on the `checksums-fake` branch).

/// Stub of `RemotePermissions`: keeps the raw db value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemotePermissions(Option<Vec<u8>>);

impl RemotePermissions {
    pub fn from_db_value(value: &[u8]) -> Self {
        if value.is_empty() {
            Self(None)
        } else {
            Self(Some(value.to_vec()))
        }
    }
    pub fn to_db_value(&self) -> Vec<u8> {
        match &self.0 {
            None => Vec::new(),
            Some(v) if v.is_empty() => b" ".to_vec(),
            Some(v) => v.clone(),
        }
    }
    pub fn is_null(&self) -> bool {
        self.0.is_none()
    }
}
