// SPDX-FileCopyrightText: 1996 Bob Jenkins
// SPDX-License-Identifier: CC0-1.0
//
// Tests are taken from lookup2.c and lookup8.c
// by Bob Jenkins, December 1996, Public Domain.
// See http://burtleburtle.net/bob/hash/evahash.html
//
// Port of upstream test/csync/std_tests/check_std_c_jhash.c (nextcloud/desktop
// v34.0.5), plus a bit-exactness check against vectors produced by the upstream
// C header (tools/jhash-oracle).

use nc_journal::jhash::{c_jhash, c_jhash64};

const MAXPAIR: u64 = 80;
const MAXLEN: usize = 70;

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Not upstream: every vector printed by the C oracle compiled against
/// upstream `c_jhash.h` must match bit for bit.
#[test]
fn upstream_c_vectors_bit_exact() {
    let data = include_str!("fixtures/jhash_vectors.txt");
    let mut n = 0;
    for line in data.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let key = unhex(f[0]);
        let i64v = u64::from_str_radix(f[1], 16).unwrap();
        let h64 = u64::from_str_radix(f[2], 16).unwrap();
        let i32v = u32::from_str_radix(f[3], 16).unwrap();
        let h32 = u32::from_str_radix(f[4], 16).unwrap();
        assert_eq!(c_jhash64(&key, i64v), h64, "c_jhash64 {line}");
        assert_eq!(c_jhash(&key, i32v), h32, "c_jhash {line}");
        n += 1;
    }
    assert_eq!(n, 418);
}

// The upstream "trials" tests only exercise the hash: their final assertion
// (`if (z == MAXPAIR) { if (z < MAXPAIR) assert_true(z < MAXPAIR); }`) can never
// fire. They are ported faithfully, loops included.
macro_rules! trials {
    ($hash:ident, $t:ty, $mstart:expr) => {{
        let mut a = [0u8; MAXLEN + 1];
        let mut b = [0u8; MAXLEN + 1];
        for hlen in 0..MAXLEN {
            let mut z: u64 = 0;
            for i in 0..hlen {
                for j in 0..8u32 {
                    for m in $mstart..8 {
                        let (mut e, mut f, mut g, mut h, mut x, mut y) =
                            (!0 as $t, !0 as $t, !0 as $t, !0 as $t, !0 as $t, !0 as $t);
                        let mut k: u64 = 0;
                        while k < MAXPAIR {
                            a[..=hlen].fill(0);
                            b[..=hlen].fill(0);
                            a[i] ^= (k << j) as u8;
                            a[i] ^= (k >> (8 - j)) as u8;
                            let c = $hash(&a[..hlen], m);
                            b[i] ^= ((k + 1) << j) as u8;
                            b[i] ^= ((k + 1) >> (8 - j)) as u8;
                            let d = $hash(&b[..hlen], m);
                            e &= c ^ d;
                            f &= !(c ^ d);
                            g &= c;
                            h &= !c;
                            x &= d;
                            y &= !d;
                            if (e | f | g | h | x | y) == 0 {
                                break;
                            }
                            k += 2;
                        }
                        if k > z {
                            z = k;
                        }
                        if z == MAXPAIR {
                            assert!(z <= MAXPAIR);
                            return;
                        }
                    }
                }
            }
        }
    }};
}

#[test]
fn check_c_jhash_trials() {
    trials!(c_jhash, u32, 1u32);
}

#[test]
fn check_c_jhash64_trials() {
    trials!(c_jhash64, u64, 0u64);
}

const Q: &[u8] = b"This is the time for all good men to come to the aid of their country";

#[test]
fn check_c_jhash_alignment_problems() {
    let test = c_jhash(Q, 0);
    for pad in 1..=3 {
        let mut shifted = vec![b'x'; pad];
        shifted.extend_from_slice(Q);
        assert_eq!(test, c_jhash(&shifted[pad..], 0));
    }
    let mut buf = [0u8; MAXLEN + 20];
    for h in 0..8 {
        let b = 1 + h;
        for i in 0..MAXLEN {
            buf[b..b + i].fill(0);
            let r = c_jhash(&buf[b..b + i], 1);
            buf[b + i] = !0;
            buf[b - 1] = !0;
            let x = c_jhash(&buf[b..b + i], 1);
            let y = c_jhash(&buf[b..b + i], 1);
            assert!(!(r != x || r != y));
        }
    }
}

#[test]
fn check_c_jhash_null_strings() {
    let buf = [!0u8; 1];
    let mut h = 0u32;
    for _ in 0..8 {
        let t = h;
        h = c_jhash(&buf[..0], h);
        assert_ne!(t, h);
    }
}

#[test]
fn check_c_jhash64_alignment_problems() {
    let t = c_jhash64(Q, 0);
    for pad in 1..=7 {
        let mut shifted = vec![b'x'; pad];
        shifted.extend_from_slice(Q);
        assert_eq!(t, c_jhash64(&shifted[pad..], 0));
    }
    let mut buf = [0u8; MAXLEN + 20];
    for h in 0..8 {
        let b = 1 + h;
        for i in 0..MAXLEN {
            buf[b..b + i].fill(0);
            let r = c_jhash64(&buf[b..b + i], 1);
            buf[b + i] = !0;
            buf[b - 1] = !0;
            let x = c_jhash64(&buf[b..b + i], 1);
            let y = c_jhash64(&buf[b..b + i], 1);
            assert!(!(r != x || r != y));
        }
    }
}

#[test]
fn check_c_jhash64_null_strings() {
    let buf = [!0u8; 1];
    let mut h = 0u64;
    for _ in 0..8 {
        let t = h;
        h = c_jhash64(&buf[..0], h);
        assert_ne!(t, h);
    }
}
