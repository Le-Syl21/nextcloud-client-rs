// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! Transparent decompression of `Content-Encoding: gzip` / `deflate`
//! responses with the decompression safety check of the Qt network stack
//! the upstream client runs on (`QNetworkRequest::decompressedSafetyCheckThreshold`,
//! `QDecompressHelper`).
//!
//! This reproduces the Qt behaviour the client depends on; it is not a copy
//! of the Qt code:
//!
//! - `gzip` and `deflate` are both inflated with automatic zlib/gzip header
//!   detection (`inflateInit2(MAX_WBITS + 32)`); a `deflate` body that is a
//!   raw deflate stream (no zlib header) is retried as raw deflate once;
//!   several concatenated streams are decoded one after the other.
//! - Every received compressed byte is counted, and the body is decoded as
//!   it arrives (Qt's "counting bytes" mode of asynchronous replies). Once
//!   more than the threshold has been decompressed and the ratio
//!   decompressed / compressed exceeds 40, the reply fails with
//!   `UnknownContentError` ("Decompression failed: The decompressed output
//!   exceeds the limits specified by
//!   QNetworkRequest::decompressedSafetyCheckThreshold()"). The output piece
//!   that crossed the limit is not delivered, so the receiver never gets
//!   more than the threshold from a rejected reply.
//! - A threshold of `-1` disables the check; the default is 10 MiB.

use bytes::Bytes;

use crate::transport::DecompressionError;

/// Qt's default `decompressedSafetyCheckThreshold()`: 10 MiB.
pub const DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD: i64 = 10 * 1024 * 1024;

/// The decompression ratio above which gzip/deflate output is rejected.
const MAX_RATIO: f64 = 40.0;

/// Output piece size: the check runs after each piece (Qt's counting helper
/// reads 256 KiB at a time).
const PIECE: usize = 256 * 1024;

/// `inflateInit2` window bits: 15 with zlib/gzip auto detection.
const AUTO_DETECT_WINDOW_BITS: u8 = 32 + 15;

/// The supported `Content-Encoding` values (case-insensitive), in Qt's
/// `acceptedEncoding()` order for a build without brotli and zstd.
pub const ACCEPTED_ENCODINGS: &str = "gzip, deflate";

/// Whether `encoding` is decoded transparently.
pub fn is_supported_encoding(encoding: &[u8]) -> bool {
    encoding.eq_ignore_ascii_case(b"gzip") || encoding.eq_ignore_ascii_case(b"deflate")
}

/// A streaming gzip/deflate decoder with the archive bomb check.
pub struct Decompressor {
    inflate: zlib_rs::Inflate,
    /// Raw deflate was tried after a zlib header error.
    tried_raw_deflate: bool,
    /// Every compressed byte received so far, kept until the first output
    /// so that a raw deflate retry can start from the beginning.
    head: Vec<u8>,
    produced_output: bool,
    stream_ended: bool,
    threshold: i64,
    total_compressed: u64,
    total_uncompressed: u64,
}

impl std::fmt::Debug for Decompressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decompressor")
            .field("threshold", &self.threshold)
            .field("total_compressed", &self.total_compressed)
            .field("total_uncompressed", &self.total_uncompressed)
            .finish()
    }
}

impl Decompressor {
    /// A decoder; `threshold` is the request's
    /// `decompressedSafetyCheckThreshold` (`-1` disables the check).
    pub fn new(threshold: i64) -> Self {
        Self {
            inflate: zlib_rs::Inflate::new(true, AUTO_DETECT_WINDOW_BITS),
            tried_raw_deflate: false,
            head: Vec::new(),
            produced_output: false,
            stream_ended: false,
            threshold: if threshold == -1 { i64::MAX } else { threshold },
            total_compressed: 0,
            total_uncompressed: 0,
        }
    }

    /// `isPotentialArchiveBomb()`.
    fn is_potential_archive_bomb(&self) -> bool {
        if self.total_compressed == 0 {
            return false;
        }
        if self.total_uncompressed as i128 <= self.threshold as i128 {
            return false;
        }
        let ratio = self.total_uncompressed as f64 / self.total_compressed as f64;
        ratio > MAX_RATIO
    }

    fn failed(reason: &str) -> DecompressionError {
        DecompressionError(format!("Decompression failed: {reason}"))
    }

    /// Feeds received compressed bytes; returns the decompressed pieces.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Bytes>, DecompressionError> {
        self.total_compressed += data.len() as u64;
        if !self.produced_output && !self.tried_raw_deflate {
            self.head.extend_from_slice(data);
        }
        self.inflate_input(data)
    }

    fn inflate_input(&mut self, data: &[u8]) -> Result<Vec<Bytes>, DecompressionError> {
        let mut out = Vec::new();
        let mut input = data;
        let mut piece = vec![0u8; PIECE];
        let mut filled = 0usize;
        while !input.is_empty() {
            if self.stream_ended {
                // More data after a finished stream: the next stream.
                self.inflate = zlib_rs::Inflate::new(true, AUTO_DETECT_WINDOW_BITS);
                self.stream_ended = false;
            }
            let before_in = self.inflate.total_in();
            let before_out = self.inflate.total_out();
            let result = self.inflate.decompress(
                input,
                &mut piece[filled..],
                zlib_rs::InflateFlush::NoFlush,
            );
            let consumed = (self.inflate.total_in() - before_in) as usize;
            let written = (self.inflate.total_out() - before_out) as usize;
            input = &input[consumed..];
            filled += written;
            match result {
                Err(zlib_rs::InflateError::DataError) if !self.tried_raw_deflate => {
                    // Maybe raw deflate data: start again without header.
                    self.tried_raw_deflate = true;
                    self.inflate = zlib_rs::Inflate::new(false, 15);
                    let head = std::mem::take(&mut self.head);
                    return self.inflate_input(&head);
                }
                Err(_) => return Err(Self::failed("invalid compressed data")),
                Ok(zlib_rs::Status::StreamEnd) => self.stream_ended = true,
                Ok(_) => {}
            }
            if filled == piece.len() || (filled > 0 && (input.is_empty() || self.stream_ended)) {
                self.emit_piece(&mut out, &piece[..filled])?;
                filled = 0;
            } else if consumed == 0 && written == 0 {
                break;
            }
        }
        if filled > 0 {
            self.emit_piece(&mut out, &piece[..filled])?;
        }
        Ok(out)
    }

    fn emit_piece(&mut self, out: &mut Vec<Bytes>, data: &[u8]) -> Result<(), DecompressionError> {
        self.total_uncompressed += data.len() as u64;
        if self.is_potential_archive_bomb() {
            return Err(Self::failed(
                "The decompressed output exceeds the limits specified by QNetworkRequest::decompressedSafetyCheckThreshold()",
            ));
        }
        self.produced_output = true;
        self.head = Vec::new();
        out.push(Bytes::copy_from_slice(data));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zlib(data: &[u8], level: i32, header: bool) -> Vec<u8> {
        compress(data, level, header, 15)
    }

    fn compress(data: &[u8], level: i32, header: bool, window_bits: u8) -> Vec<u8> {
        let mut out = vec![0u8; zlib_rs::compress_bound(data.len()) + 64];
        let mut deflate = zlib_rs::Deflate::new(level, header, window_bits);
        let mut input = data;
        let mut written = 0usize;
        loop {
            let before_in = deflate.total_in();
            let before_out = deflate.total_out();
            let status = deflate
                .compress(input, &mut out[written..], zlib_rs::DeflateFlush::Finish)
                .unwrap();
            input = &input[(deflate.total_in() - before_in) as usize..];
            written += (deflate.total_out() - before_out) as usize;
            if status == zlib_rs::Status::StreamEnd {
                break;
            }
        }
        out.truncate(written);
        out
    }

    fn decode_all(d: &mut Decompressor, data: &[u8], chunk: usize) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for c in data.chunks(chunk) {
            for p in d.feed(c).map_err(|e| e.0)? {
                out.extend_from_slice(&p);
            }
        }
        Ok(out)
    }

    #[test]
    fn derived_zlib_and_raw_deflate() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        for header in [true, false] {
            let compressed = zlib(&data, 6, header);
            let mut d = Decompressor::new(DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD);
            assert_eq!(decode_all(&mut d, &compressed, 1000).unwrap(), data);
        }
        // gzip, two concatenated members.
        let mut gz = compress(&data, 6, true, 16 + 15);
        gz.extend(compress(b"tail", 6, true, 16 + 15));
        let mut expected = data.clone();
        expected.extend_from_slice(b"tail");
        for chunk in [777, gz.len() - 30, gz.len()] {
            let mut d = Decompressor::new(DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD);
            assert_eq!(decode_all(&mut d, &gz, chunk).unwrap(), expected);
        }
    }

    #[test]
    fn derived_archive_bomb_threshold() {
        let data = vec![b'A'; 12 * 1024 * 1024];
        let compressed = zlib(&data, 9, true);
        let mut d = Decompressor::new(DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD);
        let mut received = 0usize;
        let mut failed = None;
        for c in compressed.chunks(16 * 1024) {
            match d.feed(c) {
                Ok(pieces) => received += pieces.iter().map(Bytes::len).sum::<usize>(),
                Err(e) => {
                    failed = Some(e.0);
                    break;
                }
            }
        }
        assert!(failed.unwrap().contains("decompressedSafetyCheckThreshold"));
        assert!(received as i64 <= DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD);
        // A higher threshold accepts it.
        let mut d = Decompressor::new(20 * 1024 * 1024);
        assert_eq!(
            decode_all(&mut d, &compressed, 16 * 1024).unwrap().len(),
            data.len()
        );
        // -1 disables the check.
        let mut d = Decompressor::new(-1);
        assert_eq!(
            decode_all(&mut d, &compressed, 16 * 1024).unwrap().len(),
            data.len()
        );
    }
}
