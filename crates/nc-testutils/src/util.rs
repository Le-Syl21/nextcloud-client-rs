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
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// `OCC::Utility::rand()`: uniform in `[0, RAND_MAX)`, `RAND_MAX = 2^31 - 1`.
pub(crate) fn rand() -> u32 {
    static STATE: Mutex<u64> = Mutex::new(0);
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if *s == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1);
        *s = (nanos as u64) ^ 0x9e37_79b9_7f4a_7c15 | 1;
    }
    // xorshift64*
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    let v = s.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33;
    (v % 0x7fff_ffff) as u32
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
/// (the `getlastmodified` format of `FakePropfindReply`).
pub fn http_date(t: SystemTime) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = secs_since_epoch(t);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
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
