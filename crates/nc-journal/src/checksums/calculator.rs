/*
 * SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

//! Streaming checksum calculation over a file.
//!
//! Port of upstream `src/common/checksumcalculator.{h,cpp}` and
//! `src/common/checksumconsts.h` (nextcloud/desktop v34.0.5). Both upstream
//! files are GPL-2.0-or-later, unlike the rest of `src/common`.
//!
//! Output encoding (exactly as upstream):
//! - MD5 / SHA1 / SHA256 / SHA3-256: lowercase hex of the digest;
//! - Adler32: `QByteArray::number(adler, 16)`, i.e. lowercase hex **without
//!   leading zeros** (an empty file gives `"1"`).
//!
//! The type name match is exact and case-sensitive (`"Adler32"`, not
//! `"ADLER32"`); an unknown type yields `None` (upstream: a null array).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::Digest;

use super::to_hex;

/// Tags for checksum header values (`checkSumMD5C`).
pub const CHECKSUM_MD5_C: &[u8] = b"MD5";
/// `checkSumSHA1C`.
pub const CHECKSUM_SHA1_C: &[u8] = b"SHA1";
/// `checkSumSHA2C`.
pub const CHECKSUM_SHA2_C: &[u8] = b"SHA256";
/// `checkSumSHA3C`.
pub const CHECKSUM_SHA3_C: &[u8] = b"SHA3-256";
/// `checkSumAdlerC`.
pub const CHECKSUM_ADLER_C: &[u8] = b"Adler32";

const LOG_TARGET: &str = "nextcloud.common.checksumcalculator";
const BUF_SIZE: usize = 500 * 1024;

/// `ChecksumCalculator::AlgorithmType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgorithmType {
    Undefined,
    Md5,
    Sha1,
    Sha256,
    Sha3_256,
    Adler32,
}

impl AlgorithmType {
    /// Maps a checksum type name to the algorithm (exact match).
    pub fn from_type_name(name: &[u8]) -> Self {
        match name {
            CHECKSUM_MD5_C => Self::Md5,
            CHECKSUM_SHA1_C => Self::Sha1,
            CHECKSUM_SHA2_C => Self::Sha256,
            CHECKSUM_SHA3_C => Self::Sha3_256,
            CHECKSUM_ADLER_C => Self::Adler32,
            _ => Self::Undefined,
        }
    }
}

#[allow(clippy::large_enum_variant)] // one short-lived value per calculation
enum State {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha3_256(sha3::Sha3_256),
    Adler32(adler2::Adler32),
}

impl State {
    fn new(algorithm: AlgorithmType) -> Option<Self> {
        Some(match algorithm {
            AlgorithmType::Undefined => return None,
            AlgorithmType::Md5 => Self::Md5(md5::Md5::new()),
            AlgorithmType::Sha1 => Self::Sha1(sha1::Sha1::new()),
            AlgorithmType::Sha256 => Self::Sha256(sha2::Sha256::new()),
            AlgorithmType::Sha3_256 => Self::Sha3_256(sha3::Sha3_256::new()),
            AlgorithmType::Adler32 => Self::Adler32(adler2::Adler32::new()),
        })
    }

    fn add_chunk(&mut self, chunk: &[u8]) {
        match self {
            Self::Md5(h) => h.update(chunk),
            Self::Sha1(h) => h.update(chunk),
            Self::Sha256(h) => h.update(chunk),
            Self::Sha3_256(h) => h.update(chunk),
            Self::Adler32(h) => h.write_slice(chunk),
        }
    }

    fn result(self) -> Vec<u8> {
        match self {
            Self::Md5(h) => to_hex(&h.finalize()),
            Self::Sha1(h) => to_hex(&h.finalize()),
            Self::Sha256(h) => to_hex(&h.finalize()),
            Self::Sha3_256(h) => to_hex(&h.finalize()),
            Self::Adler32(h) => format!("{:x}", h.checksum()).into_bytes(),
        }
    }
}

/// Computes one checksum of one file (`OCC::ChecksumCalculator`).
///
/// Upstream protects the device with a mutex so that destroying the
/// calculator while a worker thread computes stops the computation and makes
/// it return a null array. Here the same is achieved with a cancel flag
/// ([`cancel_handle`](Self::cancel_handle)): once set, `calculate` returns
/// `None`.
#[derive(Debug)]
pub struct ChecksumCalculator {
    file_path: PathBuf,
    algorithm: AlgorithmType,
    canceled: Arc<AtomicBool>,
}

impl ChecksumCalculator {
    pub fn new(file_path: impl AsRef<Path>, checksum_type_name: &[u8]) -> Self {
        let algorithm = AlgorithmType::from_type_name(checksum_type_name);
        if algorithm == AlgorithmType::Undefined {
            log::warn!(target: LOG_TARGET, "_algorithmType is Undefined, impossible to init Checksum Algorithm");
        }
        Self {
            file_path: file_path.as_ref().to_path_buf(),
            algorithm,
            canceled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn algorithm(&self) -> AlgorithmType {
        self.algorithm
    }

    /// Shared flag; storing `true` aborts a running or future `calculate`.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.canceled)
    }

    /// Reads the whole file in 500 KiB chunks and returns the checksum, or
    /// `None` if the type is unknown, the file cannot be opened or the
    /// calculation was canceled.
    pub fn calculate(&self) -> Option<Vec<u8>> {
        let state = State::new(self.algorithm)?;
        let file = match File::open(&self.file_path) {
            Ok(file) => file,
            Err(err) => {
                log::warn!(target: LOG_TARGET, "Could not open file {:?} for reading to compute a checksum {err}", self.file_path);
                return None;
            }
        };
        calculate_reader(state, file, &self.canceled)
    }

    /// Computes the checksum of an arbitrary reader (in-memory data, test
    /// payloads). `None` for an unknown type or on cancellation.
    pub fn calculate_from_reader(checksum_type_name: &[u8], reader: impl Read) -> Option<Vec<u8>> {
        let state = State::new(AlgorithmType::from_type_name(checksum_type_name))?;
        calculate_reader(state, reader, &AtomicBool::new(false))
    }
}

fn calculate_reader(
    mut state: State,
    mut reader: impl Read,
    canceled: &AtomicBool,
) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        if canceled.load(Ordering::Relaxed) {
            return None;
        }
        // Upstream stops at the first read that returns <= 0 (error or EOF)
        // and still returns the hash of what was read so far.
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        state.add_chunk(&buf[..n]);
    }
    if canceled.load(Ordering::Relaxed) {
        return None;
    }
    Some(state.result())
}
