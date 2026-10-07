/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! Port of upstream `test/testchecksumvalidator.cpp` (v34.0.5), test names
//! kept 1:1 (snake_case), plus derived tests for the header helpers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;

use super::*;

/// `Utility::writeRandomFile`: up to 100 KiB of random 7-bit characters.
fn write_random_file(path: &Path) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let size = (next() % (10 * 10 * 1024)) as usize;
    let data: Vec<u8> = (0..size).map(|_| (next() % 128) as u8).collect();
    std::fs::write(path, data).unwrap();
}

fn shell_sum(cmd: &str, file: &Path) -> Option<Vec<u8>> {
    let out = Command::new(cmd).arg(file).output().ok()?;
    let mut sum = out.stdout;
    let end = sum.iter().position(|&c| c == b' ')?;
    sum.truncate(end);
    Some(sum)
}

struct Fixture {
    root: tempfile::TempDir,
    testfile: PathBuf,
}

/// `initTestCase`
fn init_test_case() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let testfile = root.path().join("csFile");
    write_random_file(&testfile);
    Fixture { root, testfile }
}

#[test]
fn test_md5_calc() {
    let f = init_test_case();
    let file = f.root.path().join("file_a.bin");
    write_random_file(&file);
    assert!(file.exists());
    let sum = ChecksumCalculator::new(&file, CHECKSUM_MD5_C)
        .calculate()
        .unwrap();
    let Some(s_sum) = shell_sum("md5sum", &file) else {
        eprintln!("SKIP: Couldn't execute md5sum to calculate checksum, executable missing?");
        return;
    };
    assert!(!sum.is_empty());
    assert_eq!(s_sum, sum);
}

#[test]
fn test_sha1_calc() {
    let f = init_test_case();
    let file = f.root.path().join("file_b.bin");
    write_random_file(&file);
    assert!(file.exists());
    let sum = ChecksumCalculator::new(&file, CHECKSUM_SHA1_C)
        .calculate()
        .unwrap();
    let Some(s_sum) = shell_sum("sha1sum", &file) else {
        eprintln!("SKIP: Couldn't execute sha1sum to calculate checksum, executable missing?");
        return;
    };
    assert!(!sum.is_empty());
    assert_eq!(s_sum, sum);
}

fn upload_checksumming(checksum_type: &[u8]) {
    let f = init_test_case();
    let mut vali = ComputeChecksum::new();
    vali.set_checksum_type(checksum_type);
    let expected = ChecksumCalculator::new(&f.testfile, checksum_type)
        .calculate()
        .unwrap();
    // slotUpValidated
    let (ty, checksum) = vali.start(&f.testfile);
    assert_eq!(expected, checksum);
    assert_eq!(checksum_type, ty.as_slice());
}

#[test]
fn test_upload_checksumming_adler() {
    upload_checksumming(b"Adler32");
}

#[test]
fn test_upload_checksumming_md5() {
    upload_checksumming(CHECKSUM_MD5_C);
}

#[test]
fn test_upload_checksumming_sha1() {
    upload_checksumming(CHECKSUM_SHA1_C);
}

#[test]
fn test_download_checksumming_adler() {
    let f = init_test_case();
    let mut vali = ValidateChecksumHeader::new();
    let expected = ChecksumCalculator::new(&f.testfile, CHECKSUM_ADLER_C)
        .calculate()
        .unwrap();
    let adler = make_checksum_header(CHECKSUM_ADLER_C, &expected);
    assert!(vali.start(&f.testfile, &adler).is_ok());

    let err = vali.start(&f.testfile, b"Adler32:543345").unwrap_err();
    assert_eq!(
        err.message,
        format!(
            "The downloaded file does not match the checksum, it will be resumed. \"543345\" != \"{}\"",
            String::from_utf8_lossy(&expected)
        )
    );
    assert_eq!(err.reason, FailureReason::ChecksumMismatch);

    let err = vali.start(&f.testfile, b"Klaas32:543345").unwrap_err();
    assert_eq!(
        err.message,
        "The checksum header contained an unknown checksum type \"Klaas32\""
    );
    assert_eq!(err.reason, FailureReason::ChecksumTypeUnknown);
}

/// Upstream deletes a running `ComputeChecksum` on `/dev/zero` 4096 times;
/// the Rust equivalent is canceling a calculation over an endless reader.
#[cfg(target_os = "linux")]
#[test]
fn test_deleting_compute_checksum_before_calculation_completion_does_not_crash() {
    for _ in 0..64 {
        let calculator = ChecksumCalculator::new("/dev/zero", b"MD5");
        let cancel = calculator.cancel_handle();
        let worker = std::thread::spawn(move || calculator.calculate());
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(worker.join().unwrap(), None);
    }
}

// ---- Derived tests (no upstream counterpart), from checksums.cpp semantics ----

#[test]
fn derived_find_best_checksum() {
    assert_eq!(
        find_best_checksum(b"ADLER32:1231 SHA1:ab124124 MD5:2131affa21"),
        b"SHA1:ab124124"
    );
    assert_eq!(find_best_checksum(b"md5:aa sha256:bb"), b"sha256:bb");
    assert_eq!(find_best_checksum(b"SHA3-256:cc SHA256:bb"), b"SHA3-256:cc");
    // owncloud/core#38304: value terminated by '<'
    assert_eq!(find_best_checksum(b"MD5:abc</oc:checksum>"), b"MD5:abc");
    assert_eq!(find_best_checksum(b"Klaas32:abc"), b"");
    assert_eq!(find_best_checksum(b""), b"");
}

#[test]
fn derived_checksum_header_helpers() {
    assert_eq!(make_checksum_header(b"SHA1", b"abc"), b"SHA1:abc");
    assert_eq!(make_checksum_header(b"", b"abc"), b"");
    assert_eq!(make_checksum_header(b"SHA1", b""), b"");
    assert_eq!(parse_checksum_header(b""), Some((vec![], vec![])));
    assert_eq!(
        parse_checksum_header(b"SHA1:a:b"),
        Some((b"SHA1".to_vec(), b"a:b".to_vec()))
    );
    assert_eq!(parse_checksum_header(b"nocolon"), None);
    assert_eq!(parse_checksum_header_type(b"MD5:x"), b"MD5");
    assert_eq!(parse_checksum_header_type(b"MD5"), b"");
    assert_eq!(calc_sha256(b""), b"");
    assert_eq!(
        calc_sha256(b"abc"),
        b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn derived_calculator_encodings() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(
        ChecksumCalculator::new(&empty, b"Adler32")
            .calculate()
            .unwrap(),
        b"1"
    );
    assert_eq!(
        ChecksumCalculator::new(&empty, b"MD5").calculate().unwrap(),
        b"d41d8cd98f00b204e9800998ecf8427e"
    );
    let abc = dir.path().join("abc");
    std::fs::write(&abc, b"abc").unwrap();
    // adler32("abc") = 0x024d0127: no leading zero
    assert_eq!(
        ChecksumCalculator::new(&abc, b"Adler32")
            .calculate()
            .unwrap(),
        b"24d0127"
    );
    assert_eq!(
        ChecksumCalculator::new(&abc, b"SHA3-256")
            .calculate()
            .unwrap(),
        b"3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
    );
    assert_eq!(ChecksumCalculator::new(&abc, b"ADLER32").calculate(), None);
    assert_eq!(
        ChecksumCalculator::new(dir.path().join("missing"), b"MD5").calculate(),
        None
    );
}

#[test]
fn derived_validate_malformed_and_empty() {
    let mut vali = ValidateChecksumHeader::new();
    assert_eq!(
        vali.start(Path::new("/nonexistent"), b""),
        Ok((vec![], vec![]))
    );
    let err = vali
        .start(Path::new("/nonexistent"), b"garbage")
        .unwrap_err();
    assert_eq!(err.reason, FailureReason::ChecksumHeaderMalformed);
    assert_eq!(err.message, "The checksum header is malformed.");
}

#[test]
fn derived_csync_checksum_hook() {
    let dir = tempfile::tempdir().unwrap();
    let abc = dir.path().join("abc");
    std::fs::write(&abc, b"abc").unwrap();
    assert_eq!(
        csync_checksum_hook(&abc, b"MD5:whatever").unwrap(),
        b"MD5:900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(csync_checksum_hook(&abc, b""), None);
    assert_eq!(csync_checksum_hook(&abc, b"Klaas32:x"), None);
}
