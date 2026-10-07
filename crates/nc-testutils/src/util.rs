/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-FileCopyrightText: 2020 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! Small helpers: random numbers (`Utility::rand`), HTTP dates and a
//! minimal executor for the in-process transport.

use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

/// `OCC::Utility::rand()`: uniform in `[0, RAND_MAX)`, `RAND_MAX = 2^31 - 1`.
pub(crate) fn rand() -> u32 {
    fastrand::u32(0..0x7fff_ffff)
}

/// Milliseconds since the Unix epoch (negative before 1970).
pub(crate) fn msecs_since_epoch(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Seconds since the Unix epoch, truncated like `QDateTime::toSecsSinceEpoch`.
pub(crate) fn secs_since_epoch(t: SystemTime) -> i64 {
    msecs_since_epoch(t).div_euclid(1000)
}

pub(crate) fn from_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs())
    }
}

/// Formats like `QLocale::c().toString(utc, "ddd, dd MMM yyyy HH:mm:ss 'GMT'")`
/// (the `getlastmodified` format of `FakePropfindReply`), i.e. the IMF-fixdate
/// of RFC 9110, via the `httpdate` crate. Dates before 1970 are clamped to the
/// epoch (`httpdate` does not represent them; the fake never produces them).
pub fn http_date(t: SystemTime) -> String {
    httpdate::fmt_http_date(t.max(UNIX_EPOCH))
}

/// Drives a future that is expected to be immediately ready, which is the
/// case of every [`crate::FakeServer`] reply except
/// [`crate::FakeReply::Hang`]. Panics if the future is pending.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("block_on: future is pending (hanging reply?)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date_format() {
        assert_eq!(http_date(from_secs(0)), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            http_date(from_secs(1_791_368_496)),
            "Wed, 07 Oct 2026 10:21:36 GMT"
        );
        assert_eq!(
            http_date(from_secs(951_782_400)),
            "Tue, 29 Feb 2000 00:00:00 GMT"
        );
    }

    #[test]
    fn rand_in_range() {
        for _ in 0..1000 {
            assert!(rand() < 0x7fff_ffff);
        }
    }
}
