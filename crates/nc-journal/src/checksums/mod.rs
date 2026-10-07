/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2015 ownCloud GmbH
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

//! Computing and validating file checksums.
//!
//! Port of upstream `src/common/checksums.{h,cpp}` (nextcloud/desktop v34.0.5).
//!
//! Checksums are used in two distinct ways during synchronization:
//!
//! - to guard uploads and downloads against data corruption
//!   (transmission checksum, sent and received in the `OC-Checksum` header
//!   as `TYPE:CHECKSUM`, never stored in the database);
//! - to quickly check whether the content of a file has changed to avoid
//!   redundant uploads (content checksum, stored in the journal, never sent
//!   to the server).
//!
//! Supported algorithms: Adler32, MD5, SHA1, SHA256, SHA3-256 (see
//! [`calculator`]).
//!
//! # Mapping to upstream
//!
//! | Upstream | Here |
//! |---|---|
//! | `findBestChecksum` | [`find_best_checksum`] |
//! | `makeChecksumHeader` | [`make_checksum_header`] |
//! | `parseChecksumHeader` | [`parse_checksum_header`] |
//! | `parseChecksumHeaderType` | [`parse_checksum_header_type`] |
//! | `uploadChecksumEnabled` | [`upload_checksum_enabled`] |
//! | `calcSha256` | [`calc_sha256`] |
//! | `ComputeChecksum` (Qt object + thread) | [`ComputeChecksum`] (synchronous; the caller picks the thread) |
//! | `ComputeChecksum::computeNow(OnFile)` | [`ComputeChecksum::compute_now`] / [`ComputeChecksum::compute_now_on_file`] |
//! | `ValidateChecksumHeader` (signals) | [`ValidateChecksumHeader`] returning a `Result` |
//! | `CSyncChecksumHook::hook` | [`csync_checksum_hook`] |
//!
//! Checksums, types and headers are byte strings (`Vec<u8>`), like the
//! upstream `QByteArray`s. A *null* upstream `QByteArray` (computation could
//! not run) is `None` here; an empty one is an empty `Vec`.

pub mod calculator;

use std::path::Path;
use std::sync::OnceLock;

use sha2::Digest;

pub use calculator::{
    AlgorithmType, CHECKSUM_ADLER_C, CHECKSUM_MD5_C, CHECKSUM_SHA1_C, CHECKSUM_SHA2_C,
    CHECKSUM_SHA3_C, ChecksumCalculator,
};

const LOG_TARGET: &str = "nextcloud.sync.checksums";

/// Hex-encodes (lowercase) like `QByteArray::toHex()`.
pub(crate) fn to_hex(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize]);
        out.push(DIGITS[(b & 0xf) as usize]);
    }
    out
}

/// SHA-256 of `data` as lowercase hex; empty when `data` is empty (upstream
/// `calcSha256` returns an empty array for empty input).
pub fn calc_sha256(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    to_hex(&sha2::Sha256::digest(data))
}

/// Creates a checksum header (`TYPE:CHECKSUM`) from type and value. Empty if
/// either part is empty.
pub fn make_checksum_header(checksum_type: &[u8], checksum: &[u8]) -> Vec<u8> {
    if checksum_type.is_empty() || checksum.is_empty() {
        return Vec::new();
    }
    let mut header = Vec::with_capacity(checksum_type.len() + 1 + checksum.len());
    header.extend_from_slice(checksum_type);
    header.push(b':');
    header.extend_from_slice(checksum);
    header
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|&i| haystack[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Returns the highest-quality checksum in a `checksums` property retrieved
/// from the server.
///
/// Example: `"ADLER32:1231 SHA1:ab124124 MD5:2131affa21"` → `"SHA1:ab124124"`.
///
/// Preference order: SHA3-256, SHA256, SHA1, MD5, ADLER32 (case-insensitive
/// search). The value extends to the next space, or else to the next `<`
/// (workaround for owncloud/core#38304), or else to the end.
pub fn find_best_checksum(checksums: &[u8]) -> Vec<u8> {
    if checksums.is_empty() {
        return Vec::new();
    }
    // The order of the searches here defines the preference ordering.
    const PREFERENCE: [&[u8]; 5] = [b"SHA3-256:", b"SHA256:", b"SHA1:", b"MD5:", b"ADLER32:"];
    for needle in PREFERENCE {
        if let Some(i) = find_ascii_case_insensitive(checksums, needle) {
            let rest = &checksums[i..];
            let end = rest
                .iter()
                .position(|&c| c == b' ')
                .or_else(|| rest.iter().position(|&c| c == b'<'))
                .unwrap_or(rest.len());
            return rest[..end].to_vec();
        }
    }
    log::warn!(target: LOG_TARGET, "Failed to parse {:?}", String::from_utf8_lossy(checksums));
    Vec::new()
}

/// Parses a checksum header into `(type, checksum)`.
///
/// An empty header is valid and yields two empty parts; a header without a
/// `:` is malformed (`None`).
pub fn parse_checksum_header(header: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if header.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }
    let idx = header.iter().position(|&c| c == b':')?;
    Some((header[..idx].to_vec(), header[idx + 1..].to_vec()))
}

/// Convenience for getting the type from a checksum header, empty if none.
pub fn parse_checksum_header_type(header: &[u8]) -> Vec<u8> {
    match header.iter().position(|&c| c == b':') {
        Some(idx) => header[..idx].to_vec(),
        None => Vec::new(),
    }
}

fn env_is_empty(name: &str) -> bool {
    // qEnvironmentVariableIsEmpty: true when unset or set to an empty value.
    std::env::var_os(name).is_none_or(|v| v.is_empty())
}

/// Checks `OWNCLOUD_DISABLE_CHECKSUM_UPLOAD` (read once per process, like
/// upstream's function-local static).
pub fn upload_checksum_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_is_empty("OWNCLOUD_DISABLE_CHECKSUM_UPLOAD"))
}

/// Checks `OWNCLOUD_DISABLE_CHECKSUM_COMPUTATIONS` (read once per process).
fn checksum_computation_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_is_empty("OWNCLOUD_DISABLE_CHECKSUM_COMPUTATIONS"))
}

/// Computes the checksum of a file.
///
/// Upstream runs the computation on a `QtConcurrent` thread and emits
/// `done(type, checksum)`; here [`ComputeChecksum::start`] is synchronous and
/// returns what `done` would carry. The async engine is expected to call it
/// from a blocking task (`tokio::task::spawn_blocking`) and to cancel through
/// [`ChecksumCalculator::cancel_handle`] if needed.
#[derive(Debug, Default, Clone)]
pub struct ComputeChecksum {
    checksum_type: Vec<u8>,
}

impl ComputeChecksum {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the checksum type to be used. The default is empty.
    pub fn set_checksum_type(&mut self, checksum_type: &[u8]) {
        self.checksum_type = checksum_type.to_vec();
    }

    pub fn checksum_type(&self) -> &[u8] {
        &self.checksum_type
    }

    /// Computes the checksum for the given file path and returns the
    /// `(checksumType, checksum)` pair upstream's `done` signal carries:
    /// `(type, checksum)` on success, `(empty, empty)` when the computation
    /// could not run (unknown type, unreadable file, cancellation).
    ///
    /// Like upstream's asynchronous path, this ignores
    /// `OWNCLOUD_DISABLE_CHECKSUM_COMPUTATIONS` (only `compute_now` honours it).
    pub fn start(&self, file_path: &Path) -> (Vec<u8>, Vec<u8>) {
        log::debug!(target: LOG_TARGET, "Computing {:?} checksum of {:?}", String::from_utf8_lossy(&self.checksum_type), file_path);
        let calculator = ChecksumCalculator::new(file_path, &self.checksum_type);
        Self::done(&self.checksum_type, calculator.calculate())
    }

    /// Same as [`start`](Self::start) but with a caller-provided calculator
    /// (e.g. one whose cancel handle was handed to another thread).
    pub fn start_with(&self, calculator: &ChecksumCalculator) -> (Vec<u8>, Vec<u8>) {
        Self::done(&self.checksum_type, calculator.calculate())
    }

    fn done(checksum_type: &[u8], checksum: Option<Vec<u8>>) -> (Vec<u8>, Vec<u8>) {
        match checksum {
            Some(checksum) => (checksum_type.to_vec(), checksum),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// Computes the checksum synchronously. `None` when disabled by
    /// `OWNCLOUD_DISABLE_CHECKSUM_COMPUTATIONS` or when the computation could
    /// not run.
    pub fn compute_now(file_path: &Path, checksum_type: &[u8]) -> Option<Vec<u8>> {
        if !checksum_computation_enabled() {
            log::warn!(target: LOG_TARGET, "Checksum computation disabled by environment variable");
            return None;
        }
        ChecksumCalculator::new(file_path, checksum_type).calculate()
    }

    /// Convenience wrapper for [`compute_now`](Self::compute_now).
    pub fn compute_now_on_file(file_path: &Path, checksum_type: &[u8]) -> Option<Vec<u8>> {
        Self::compute_now(file_path, checksum_type)
    }
}

/// Why a checksum validation failed (`ValidateChecksumHeader::FailureReason`).
/// `Success` has no counterpart: success is `Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    ChecksumHeaderMalformed,
    ChecksumTypeUnknown,
    ChecksumMismatch,
}

/// Payload of upstream's `validationFailed` signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationFailure {
    /// User-visible message, same English text as upstream (untranslated).
    pub message: String,
    pub calculated_checksum_type: Vec<u8>,
    pub calculated_checksum: Vec<u8>,
    pub reason: FailureReason,
}

/// Checks whether a file's checksum matches the expected value.
///
/// `Ok((type, checksum))` corresponds to upstream's `validated` signal
/// (`(empty, empty)` when the header is empty: nothing to validate),
/// `Err` to `validationFailed`.
#[derive(Debug, Default, Clone)]
pub struct ValidateChecksumHeader {
    calculated_checksum_type: Vec<u8>,
    calculated_checksum: Vec<u8>,
}

impl ValidateChecksumHeader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks the file's actual checksum against `checksum_header`.
    pub fn start(
        &mut self,
        file_path: &Path,
        checksum_header: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), ValidationFailure> {
        self.start_with(checksum_header, |checksum_type| {
            let mut compute = ComputeChecksum::new();
            compute.set_checksum_type(checksum_type);
            compute.start(file_path)
        })
    }

    /// Same as [`start`](Self::start) with a custom computation returning the
    /// `(type, checksum)` pair of `ComputeChecksum::done` (used for in-memory
    /// data and for tests).
    pub fn start_with(
        &mut self,
        checksum_header: &[u8],
        compute: impl FnOnce(&[u8]) -> (Vec<u8>, Vec<u8>),
    ) -> Result<(Vec<u8>, Vec<u8>), ValidationFailure> {
        // If the incoming header is empty no validation can happen. Just continue.
        if checksum_header.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let Some((expected_type, expected_checksum)) = parse_checksum_header(checksum_header)
        else {
            log::warn!(target: LOG_TARGET, "Checksum header malformed: {:?}", String::from_utf8_lossy(checksum_header));
            return Err(ValidationFailure {
                message: "The checksum header is malformed.".to_owned(),
                calculated_checksum_type: self.calculated_checksum_type.clone(),
                calculated_checksum: self.calculated_checksum.clone(),
                reason: FailureReason::ChecksumHeaderMalformed,
            });
        };
        let (checksum_type, checksum) = compute(&expected_type);
        self.calculated_checksum_type = checksum_type.clone();
        self.calculated_checksum = checksum.clone();

        if checksum_type != expected_type {
            return Err(ValidationFailure {
                message: format!(
                    "The checksum header contained an unknown checksum type \"{}\"",
                    String::from_utf8_lossy(&expected_type)
                ),
                calculated_checksum_type: checksum_type,
                calculated_checksum: checksum,
                reason: FailureReason::ChecksumTypeUnknown,
            });
        }
        if checksum != expected_checksum {
            return Err(ValidationFailure {
                message: format!(
                    "The downloaded file does not match the checksum, it will be resumed. \"{}\" != \"{}\"",
                    String::from_utf8_lossy(&expected_checksum),
                    String::from_utf8_lossy(&checksum)
                ),
                calculated_checksum_type: checksum_type,
                calculated_checksum: checksum,
                reason: FailureReason::ChecksumMismatch,
            });
        }
        Ok((checksum_type, checksum))
    }

    pub fn calculated_checksum_type(&self) -> &[u8] {
        &self.calculated_checksum_type
    }

    pub fn calculated_checksum(&self) -> &[u8] {
        &self.calculated_checksum
    }
}

/// Returns the checksum header for `path` that is comparable to
/// `other_checksum_header` (same type), or `None` (`CSyncChecksumHook::hook`).
pub fn csync_checksum_hook(path: &Path, other_checksum_header: &[u8]) -> Option<Vec<u8>> {
    let checksum_type = parse_checksum_header_type(other_checksum_header);
    if checksum_type.is_empty() {
        return None;
    }
    log::info!(target: LOG_TARGET, "Computing {:?} checksum of {:?} in the csync hook", String::from_utf8_lossy(&checksum_type), path);
    let Some(checksum) = ComputeChecksum::compute_now_on_file(path, &checksum_type) else {
        log::warn!(target: LOG_TARGET, "Failed to compute checksum {:?} for {:?}", String::from_utf8_lossy(&checksum_type), path);
        return None;
    };
    Some(make_checksum_header(&checksum_type, &checksum))
}

#[cfg(test)]
mod tests;
