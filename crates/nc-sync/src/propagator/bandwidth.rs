// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/bandwidthmanager.{h,cpp}` (nextcloud/desktop v34.0.5),
// plus the bandwidth parts of `UploadDevice` (propagateupload.cpp) and
// `GETFileJob` (propagatedownload.cpp), and `OwncloudPropagator::_uploadLimit`
// / `_downloadLimit`.

//! The bandwidth manager.
//!
//! Upstream v34.0.5 only has absolute limits: a positive limit (bytes per
//! second; `nextcloudcmd --uplimit N` passes `N * 1000`) is shared every
//! second between the registered upload devices / download jobs, each one
//! getting `limit / count` bytes of quota for the next second (unused quota
//! is not carried over). A zero or negative limit means no rate limit (the
//! relative, percent-based limits of older clients were removed upstream);
//! any non-zero limit still serializes the transfers
//! (`maximumActiveTransferJob() == 1`).
//!
//! Mechanics: upstream's `QTimer`s (the 10 s switching timer that re-reads
//! the propagator's limits, and the 1 s absolute limit timer) are driven by
//! the propagator loop ([`super::Propagator::run`]). A device that is out of
//! quota waits on a [`tokio::sync::Notify`] where upstream's `readData()` /
//! `slotReadyRead()` returned 0 and waited for the next `readyRead`. The
//! state is atomic because upload bodies are streamed by the HTTP client's
//! connection task.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LOG: &str = "nextcloud.sync.bandwidthmanager";

/// `_switchingTimer` interval.
pub const SWITCHING_TIMER_INTERVAL: Duration = Duration::from_secs(10);
/// `_absoluteLimitTimer` interval.
pub const ABSOLUTE_LIMIT_TIMER_INTERVAL: Duration = Duration::from_secs(1);

/// `OwncloudPropagator::_uploadLimit` / `_downloadLimit` (bytes per second;
/// 0: no limit). Shared with the sync engine so that `setNetworkLimits` may
/// change them while a sync runs, as `Folder::setDirtyNetworkLimits` does.
#[derive(Debug, Default)]
pub struct NetworkLimits {
    upload: AtomicI32,
    download: AtomicI32,
}

impl NetworkLimits {
    pub fn new(upload: i32, download: i32) -> Self {
        Self {
            upload: AtomicI32::new(upload),
            download: AtomicI32::new(download),
        }
    }

    /// Sets both limits.
    pub fn set(&self, upload: i32, download: i32) {
        self.upload.store(upload, Ordering::Relaxed);
        self.download.store(download, Ordering::Relaxed);
    }

    /// `_uploadLimit`.
    pub fn upload(&self) -> i32 {
        self.upload.load(Ordering::Relaxed)
    }

    /// `_downloadLimit`.
    pub fn download(&self) -> i32 {
        self.download.load(Ordering::Relaxed)
    }
}

/// The bandwidth state of one `UploadDevice` or `GETFileJob`.
#[derive(Debug, Default)]
pub struct BandwidthClient {
    /// `_bandwidthLimited`: if `_bandwidthQuota` will be used.
    limited: AtomicBool,
    /// `_choked` / `_bandwidthChoked`: paused.
    choked: AtomicBool,
    /// `_bandwidthQuota`.
    quota: AtomicI64,
    /// Unregistered (end of data, or the device is gone).
    unregistered: AtomicBool,
    /// `readyRead` / `slotReadyRead` invoked after a state change.
    ready: tokio::sync::Notify,
}

impl BandwidthClient {
    /// `setBandwidthLimited(b)`.
    pub fn set_bandwidth_limited(&self, b: bool) {
        self.limited.store(b, Ordering::Relaxed);
        self.ready.notify_one();
    }

    /// `setChoked(c)`.
    pub fn set_choked(&self, c: bool) {
        self.choked.store(c, Ordering::Relaxed);
        if !c {
            self.ready.notify_one();
        }
    }

    /// `giveBandwidthQuota(q)`.
    pub fn give_bandwidth_quota(&self, q: i64) {
        self.quota.store(q, Ordering::Relaxed);
        self.ready.notify_one();
    }

    pub fn is_bandwidth_limited(&self) -> bool {
        self.limited.load(Ordering::Relaxed)
    }

    pub fn is_choked(&self) -> bool {
        self.choked.load(Ordering::Relaxed)
    }

    /// The amount of bytes that may be transferred now, out of `max`
    /// (`readData` / `slotReadyRead`): 0 when choked or out of quota.
    fn take(&self, max: usize) -> usize {
        if max == 0 || self.is_choked() {
            return 0;
        }
        if !self.is_bandwidth_limited() {
            return max;
        }
        let quota = self.quota.load(Ordering::Relaxed);
        let n = (max as i64).min(quota);
        if n <= 0 {
            // no quota
            return 0;
        }
        self.quota.fetch_sub(n, Ordering::Relaxed);
        n as usize
    }

    /// Waits until at least one byte of `max` may be transferred and
    /// returns how many.
    pub async fn acquire(&self, max: usize) -> usize {
        if max == 0 {
            return 0;
        }
        loop {
            let notified = self.ready.notified();
            let n = self.take(max);
            if n > 0 {
                return n;
            }
            notified.await;
        }
    }
}

/// A registration in the manager; dropping it unregisters
/// (`QObject::destroyed` → `unregisterUploadDevice` / `unregisterDownloadJob`).
#[derive(Debug)]
pub struct BandwidthRegistration(Arc<BandwidthClient>);

impl BandwidthRegistration {
    pub fn client(&self) -> &BandwidthClient {
        &self.0
    }

    /// Unregisters now (`UploadDevice::readData` at the end of the data).
    pub fn unregister(&self) {
        self.0.unregistered.store(true, Ordering::Relaxed);
    }
}

impl Drop for BandwidthRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[derive(Debug, Default)]
struct State {
    current_upload_limit: i64,
    current_download_limit: i64,
    /// `_absoluteUploadDeviceList`.
    upload_devices: Vec<Arc<BandwidthClient>>,
    /// `_downloadJobList`.
    download_jobs: Vec<Arc<BandwidthClient>>,
}

impl State {
    fn prune(&mut self) {
        self.upload_devices
            .retain(|c| !c.unregistered.load(Ordering::Relaxed));
        self.download_jobs
            .retain(|c| !c.unregistered.load(Ordering::Relaxed));
    }

    fn using_absolute_upload_limit(&self) -> bool {
        self.current_upload_limit > 0
    }

    fn using_absolute_download_limit(&self) -> bool {
        self.current_download_limit > 0
    }
}

/// `BandwidthManager`.
#[derive(Debug)]
pub struct BandwidthManager {
    /// The propagator's limits (`_propagator->_uploadLimit` ...).
    limits: Mutex<Arc<NetworkLimits>>,
    state: Mutex<State>,
}

impl BandwidthManager {
    /// `BandwidthManager(propagator)`: starts from the current limits.
    pub fn new(limits: Arc<NetworkLimits>) -> Self {
        let state = State {
            current_upload_limit: i64::from(limits.upload()),
            current_download_limit: i64::from(limits.download()),
            ..State::default()
        };
        Self {
            limits: Mutex::new(limits),
            state: Mutex::new(state),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The propagator's limits.
    pub fn limits(&self) -> Arc<NetworkLimits> {
        self.limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replaces the limits the manager follows (the engine's shared limits).
    pub fn set_limits(&self, limits: Arc<NetworkLimits>) {
        *self.limits.lock().unwrap_or_else(|e| e.into_inner()) = limits;
    }

    /// `usingAbsoluteUploadLimit()`.
    pub fn using_absolute_upload_limit(&self) -> bool {
        self.state().using_absolute_upload_limit()
    }

    /// `usingAbsoluteDownloadLimit()`.
    pub fn using_absolute_download_limit(&self) -> bool {
        self.state().using_absolute_download_limit()
    }

    /// `registerUploadDevice(p)`.
    pub fn register_upload_device(&self) -> BandwidthRegistration {
        let client = Arc::new(BandwidthClient::default());
        let mut st = self.state();
        st.prune();
        st.upload_devices.push(client.clone());
        client.set_bandwidth_limited(st.using_absolute_upload_limit());
        client.set_choked(false);
        BandwidthRegistration(client)
    }

    /// `registerDownloadJob(j)`.
    pub fn register_download_job(&self) -> BandwidthRegistration {
        let client = Arc::new(BandwidthClient::default());
        let mut st = self.state();
        st.prune();
        st.download_jobs.push(client.clone());
        client.set_bandwidth_limited(st.using_absolute_download_limit());
        client.set_choked(false);
        BandwidthRegistration(client)
    }

    /// `switchingTimerExpired()`: picks up changed limits.
    pub fn switching_timer_expired(&self) {
        let limits = self.limits();
        let mut st = self.state();
        st.prune();
        let new_upload_limit = i64::from(limits.upload());
        if new_upload_limit != st.current_upload_limit {
            log::info!(target: LOG, "Upload Bandwidth limit changed {} {new_upload_limit}", st.current_upload_limit);
            st.current_upload_limit = new_upload_limit;
            let limited = st.using_absolute_upload_limit();
            for device in &st.upload_devices {
                device.set_bandwidth_limited(limited);
                device.set_choked(false);
            }
        }
        let new_download_limit = i64::from(limits.download());
        if new_download_limit != st.current_download_limit {
            log::info!(target: LOG, "Download Bandwidth limit changed {} {new_download_limit}", st.current_download_limit);
            st.current_download_limit = new_download_limit;
            let limited = st.using_absolute_download_limit();
            for job in &st.download_jobs {
                job.set_bandwidth_limited(limited);
                job.set_choked(false);
            }
        }
    }

    /// `absoluteLimitTimerExpired()`: hands out the next second's quotas.
    pub fn absolute_limit_timer_expired(&self) {
        let mut st = self.state();
        st.prune();
        if st.using_absolute_upload_limit() && !st.upload_devices.is_empty() {
            let quota_per_device = st.current_upload_limit / st.upload_devices.len().max(1) as i64;
            log::debug!(target: LOG, "{quota_per_device} {} {}", st.upload_devices.len(), st.current_upload_limit);
            for device in &st.upload_devices {
                device.give_bandwidth_quota(quota_per_device);
                log::debug!(target: LOG, "Gave {} kB to upload device", quota_per_device as f64 / 1024.0);
            }
        }
        if st.using_absolute_download_limit() && !st.download_jobs.is_empty() {
            let quota_per_job = st.current_download_limit / st.download_jobs.len().max(1) as i64;
            log::debug!(target: LOG, "{quota_per_job} {} {}", st.download_jobs.len(), st.current_download_limit);
            for job in &st.download_jobs {
                job.give_bandwidth_quota(quota_per_job);
                log::debug!(target: LOG, "Gave {} kB to download job", quota_per_job as f64 / 1024.0);
            }
        }
    }

    /// Runs the manager's two timers until the returned future is dropped
    /// (upstream starts them in the constructor; its first, queued
    /// `switchingTimerExpired()` is run by the propagator when it is given
    /// its limits, before any job starts).
    pub async fn run_timers(&self) {
        use tokio::time::{Instant, MissedTickBehavior, interval_at};
        let start = Instant::now();
        let mut switching = interval_at(start + SWITCHING_TIMER_INTERVAL, SWITCHING_TIMER_INTERVAL);
        switching.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut absolute = interval_at(
            start + ABSOLUTE_LIMIT_TIMER_INTERVAL,
            ABSOLUTE_LIMIT_TIMER_INTERVAL,
        );
        absolute.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = switching.tick() => self.switching_timer_expired(),
                _ = absolute.tick() => self.absolute_limit_timer_expired(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_quota_split_between_registered_jobs() {
        let limits = Arc::new(NetworkLimits::new(0, 3000));
        let bwm = BandwidthManager::new(limits.clone());
        assert!(bwm.using_absolute_download_limit());
        assert!(!bwm.using_absolute_upload_limit());
        let a = bwm.register_download_job();
        let b = bwm.register_download_job();
        let up = bwm.register_upload_device();
        assert!(a.client().is_bandwidth_limited());
        assert!(!up.client().is_bandwidth_limited());
        // No quota before the first timer tick.
        assert_eq!(a.client().take(8192), 0);
        bwm.absolute_limit_timer_expired();
        assert_eq!(a.client().take(8192), 1500);
        assert_eq!(a.client().take(8192), 0);
        assert_eq!(b.client().take(1000), 1000);
        assert_eq!(b.client().take(1000), 500);
        // Unlimited uploads go through.
        assert_eq!(up.client().take(8192), 8192);
        // An unregistered job no longer counts.
        drop(b);
        bwm.absolute_limit_timer_expired();
        assert_eq!(a.client().take(8192), 3000);
        // A changed limit is picked up by the switching timer only.
        limits.set(-50, 0);
        assert!(bwm.using_absolute_download_limit());
        bwm.switching_timer_expired();
        assert!(!bwm.using_absolute_download_limit());
        assert!(!bwm.using_absolute_upload_limit());
        assert!(!a.client().is_bandwidth_limited());
        assert_eq!(a.client().take(10), 10);
    }

    #[test]
    fn derived_timers_pace_quota() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let bwm = Arc::new(BandwidthManager::new(Arc::new(NetworkLimits::new(1000, 0))));
            let dev = bwm.register_upload_device();
            let start = tokio::time::Instant::now();
            let timers = bwm.run_timers();
            tokio::pin!(timers);
            let mut sent = 0usize;
            while sent < 2500 {
                tokio::select! {
                    n = dev.client().acquire(2500 - sent) => sent += n,
                    _ = &mut timers => unreachable!(),
                }
            }
            // 1000 bytes at t=1s, 2000 at t=2s, 2500 at t=3s.
            assert_eq!(start.elapsed(), Duration::from_secs(3));
        });
    }
}
