// SPDX-License-Identifier: LGPL-2.1-or-later
//! TEMPORARY stub (replaced by the full port on the `checksums-fake` branch).

/// `parseChecksumHeader`.
pub fn parse_checksum_header(header: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if header.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }
    let idx = header.iter().position(|&c| c == b':')?;
    Some((header[..idx].to_vec(), header[idx + 1..].to_vec()))
}
