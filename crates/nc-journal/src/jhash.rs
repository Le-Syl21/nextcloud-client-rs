// c_jhash.c Jenkins Hash
//
// SPDX-FileCopyrightText: 2017 ownCloud GmbH
// SPDX-FileCopyrightText: 1997 Bob Jenkins <bob_jenkins@burtleburtle.net>
// SPDX-License-Identifier: GPL-2.0-or-later
//
// lookup8.c, by Bob Jenkins, January 4 1997, Public Domain.
// hash(), hash2(), hash3, and _c_mix() are externally useful functions.
// You can use this free for any purpose.  It has no warranty.
// See http://burtleburtle.net/bob/hash/evahash.html
//
// Port of upstream `src/common/c_jhash.h` (nextcloud/desktop v34.0.5).

//! Bob Jenkins' `lookup8` hash, as used by the Nextcloud client.
//!
//! [`c_jhash64`] with an initial value of `0` over the UTF-8 bytes of the
//! relative path is the `phash` primary key of the journal's `metadata` table,
//! so it must stay bit-exact with upstream. All arithmetic wraps, exactly like
//! the unsigned C arithmetic it is ported from.

#[inline(always)]
fn mix32(a: &mut u32, b: &mut u32, c: &mut u32) {
    macro_rules! step {
        ($x:ident, $y:ident, $z:ident, $op:tt, $s:expr) => {
            *$x = $x.wrapping_sub(*$y);
            *$x = $x.wrapping_sub(*$z);
            *$x ^= *$z $op $s;
        };
    }
    step!(a, b, c, >>, 13);
    step!(b, c, a, <<, 8);
    step!(c, a, b, >>, 13);
    step!(a, b, c, >>, 12);
    step!(b, c, a, <<, 16);
    step!(c, a, b, >>, 5);
    step!(a, b, c, >>, 3);
    step!(b, c, a, <<, 10);
    step!(c, a, b, >>, 15);
}

#[inline(always)]
fn mix64(a: &mut u64, b: &mut u64, c: &mut u64) {
    macro_rules! step {
        ($x:ident, $y:ident, $z:ident, $op:tt, $s:expr) => {
            *$x = $x.wrapping_sub(*$y);
            *$x = $x.wrapping_sub(*$z);
            *$x ^= *$z $op $s;
        };
    }
    step!(a, b, c, >>, 43);
    step!(b, c, a, <<, 9);
    step!(c, a, b, >>, 8);
    step!(a, b, c, >>, 38);
    step!(b, c, a, <<, 23);
    step!(c, a, b, >>, 5);
    step!(a, b, c, >>, 35);
    step!(b, c, a, <<, 49);
    step!(c, a, b, >>, 11);
    step!(a, b, c, >>, 12);
    step!(b, c, a, <<, 18);
    step!(c, a, b, >>, 22);
}

/// Hash a variable-length key into a 32-bit value (`c_jhash`).
///
/// Note: like upstream, the length is folded into `c` as a `u32`.
pub fn c_jhash(key: &[u8], initval: u32) -> u32 {
    let length = key.len() as u32;
    let mut a: u32 = 0x9e37_79b9;
    let mut b: u32 = 0x9e37_79b9;
    let mut c: u32 = initval;

    let mut chunks = key.chunks_exact(12);
    for k in &mut chunks {
        a = a.wrapping_add(u32::from_le_bytes([k[0], k[1], k[2], k[3]]));
        b = b.wrapping_add(u32::from_le_bytes([k[4], k[5], k[6], k[7]]));
        c = c.wrapping_add(u32::from_le_bytes([k[8], k[9], k[10], k[11]]));
        mix32(&mut a, &mut b, &mut c);
    }

    // Handle the last 11 bytes; the first byte of `c` is reserved for the length.
    let k = chunks.remainder();
    c = c.wrapping_add(length);
    for (i, &byte) in k.iter().enumerate() {
        let v = u32::from(byte);
        match i {
            0..=3 => a = a.wrapping_add(v << (8 * i)),
            4..=7 => b = b.wrapping_add(v << (8 * (i - 4))),
            _ => c = c.wrapping_add(v << (8 * (i - 7))),
        }
    }
    mix32(&mut a, &mut b, &mut c);
    c
}

/// Hash a variable-length key into a 64-bit value (`c_jhash64`).
pub fn c_jhash64(key: &[u8], initval: u64) -> u64 {
    let length = key.len() as u64;
    let mut a: u64 = initval;
    let mut b: u64 = initval;
    let mut c: u64 = 0x9e37_79b9_7f4a_7c13;

    let mut chunks = key.chunks_exact(24);
    for k in &mut chunks {
        let word = |o: usize| u64::from_le_bytes(k[o..o + 8].try_into().expect("8 bytes"));
        a = a.wrapping_add(word(0));
        b = b.wrapping_add(word(8));
        c = c.wrapping_add(word(16));
        mix64(&mut a, &mut b, &mut c);
    }

    // Handle the last 23 bytes; the first byte of `c` is reserved for the length.
    let k = chunks.remainder();
    c = c.wrapping_add(length);
    for (i, &byte) in k.iter().enumerate() {
        let v = u64::from(byte);
        match i {
            0..=7 => a = a.wrapping_add(v << (8 * i)),
            8..=15 => b = b.wrapping_add(v << (8 * (i - 8))),
            _ => c = c.wrapping_add(v << (8 * (i - 15))),
        }
    }
    mix64(&mut a, &mut b, &mut c);
    c
}
