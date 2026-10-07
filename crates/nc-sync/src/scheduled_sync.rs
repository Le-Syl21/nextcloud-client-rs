// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the scheduled sync run timers of upstream
// `src/libsync/syncengine.{h,cpp}` (`slotScheduleFilesDelayedSync`,
// `groupNeededScheduledSyncRuns`, `nearbyScheduledSyncTimer`,
// `slotCleanupScheduledSyncTimers`, `slotUnscheduleFilesDelayedSync`)
// (nextcloud/desktop v34.0.5).

//! Sync runs scheduled for when the server-side locks of some files expire.
//!
//! Discovery reports, for every file locked on the server with a lock
//! timeout, the number of seconds until the lock expires
//! (`_filesNeedingScheduledSync`). The engine groups them in buckets of one
//! minute and keeps one single-shot timer per bucket; when a timer fires
//! the engine starts a sync (`startSync()`).
//!
//! Upstream's `QTimer`s are modelled as deadlines: the owner of the engine
//! (the daemon) waits until [`ScheduledSyncTimers::next_deadline`] and then
//! calls [`ScheduledSyncTimers::fire_due`], which says whether a sync must
//! start. `nextcloudcmd` never runs its event loop long enough for these
//! timers to fire, and neither does `ncsync sync`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const LOG: &str = "nextcloud.sync.engine";

/// The bucket width of `slotScheduleFilesDelayedSync` (`intervalSecs`).
pub const SCHEDULED_SYNC_INTERVAL_SECS: i64 = 60;

/// `ScheduledSyncTimer`: a single-shot timer responsible for some files.
#[derive(Debug)]
struct Timer {
    files: HashSet<String>,
    /// `interval()`: the timeout of the last `start()`.
    interval: Duration,
    /// Set while the timer is active.
    deadline: Option<Instant>,
}

impl Timer {
    fn start(&mut self, now: Instant, interval: Duration) {
        self.interval = interval;
        self.deadline = Some(now + interval);
    }

    fn is_active(&self) -> bool {
        self.deadline.is_some()
    }

    /// `remainingTime()` in milliseconds (-1 when inactive, like `QTimer`).
    fn remaining_msecs(&self, now: Instant) -> i64 {
        match self.deadline {
            Some(d) => d.saturating_duration_since(now).as_millis() as i64,
            None => -1,
        }
    }
}

/// `ScheduledSyncBucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bucket {
    scheduled_sync_timer_secs: i64,
    files: Vec<String>,
}

/// `_scheduledSyncTimers` and `_filesScheduledForLaterSync`.
#[derive(Debug, Default)]
pub struct ScheduledSyncTimers {
    /// All timers still referenced (the `QSharedPointer`s), by id.
    timers: HashMap<u64, Timer>,
    /// `_scheduledSyncTimers`, in insertion order.
    list: Vec<u64>,
    /// `_filesScheduledForLaterSync`.
    files_scheduled_for_later_sync: HashMap<String, u64>,
    next_id: u64,
}

impl ScheduledSyncTimers {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of active scheduled sync run timers.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The earliest deadline of an active timer.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.list
            .iter()
            .filter_map(|id| self.timers.get(id).and_then(|t| t.deadline))
            .min()
    }

    /// Whether `file` is covered by a scheduled sync run.
    pub fn is_scheduled(&self, file: &str) -> bool {
        self.files_scheduled_for_later_sync.contains_key(file)
    }

    /// `groupNeededScheduledSyncRuns(interval)`.
    fn group_needed_scheduled_sync_runs(
        &self,
        files_needing_scheduled_sync: &HashMap<String, i64>,
        interval: i64,
    ) -> Vec<(i64, Bucket)> {
        let mut buckets: Vec<(i64, Bucket)> = Vec::new();
        let mut files: Vec<(&String, &i64)> = files_needing_scheduled_sync.iter().collect();
        // QHash iteration order is unspecified; sort for determinism.
        files.sort();
        for (file, sync_scheduled_secs) in files {
            let sync_scheduled_secs = *sync_scheduled_secs;
            // We don't want to schedule syncs again for files we have already discovered needing a
            // scheduled sync, unless the files have been re-locked or had their lock expire time
            // extended. So we check the time-out of the already set timer with the time-out we
            // receive from the server entry
            if let Some(id) = self.files_scheduled_for_later_sync.get(file)
                && let Some(t) = self.timers.get(id)
                && t.interval.as_millis() as i64 / 1000 >= sync_scheduled_secs
            {
                continue;
            }
            // Both qint64 so division results in floor-ed result
            let key = sync_scheduled_secs / interval;
            match buckets.iter_mut().find(|(k, _)| *k == key) {
                None => buckets.push((
                    key,
                    Bucket {
                        scheduled_sync_timer_secs: sync_scheduled_secs,
                        files: vec![file.clone()],
                    },
                )),
                Some((_, bucket)) => {
                    bucket.scheduled_sync_timer_secs =
                        bucket.scheduled_sync_timer_secs.max(sync_scheduled_secs);
                    bucket.files.push(file.clone());
                }
            }
        }
        buckets
    }

    /// `nearbyScheduledSyncTimer(scheduledSyncTimerSecs, intervalSecs)`.
    fn nearby_scheduled_sync_timer(
        &mut self,
        now: Instant,
        scheduled_sync_timer_secs: i64,
        interval_secs: i64,
    ) -> Option<u64> {
        let scheduled_sync_timer_msecs = scheduled_sync_timer_secs * 1000;
        let half_interval_msecs = (interval_secs * 1000) / 2;
        for id in self.list.clone() {
            let Some(timer) = self.timers.get_mut(&id) else {
                continue;
            };
            let timer_remaining_msecs = timer.remaining_msecs(now);
            let difference_msecs = timer_remaining_msecs - scheduled_sync_timer_msecs;
            let nearby_scheduled_sync =
                difference_msecs > -half_interval_msecs && difference_msecs < half_interval_msecs;
            // Iterated timer is going to fire slightly before we need it to for the parameter timer, delay it.
            if difference_msecs > -half_interval_msecs && difference_msecs < 0 {
                timer.start(
                    now,
                    Duration::from_millis(scheduled_sync_timer_msecs.max(0) as u64),
                );
                log::debug!(target: LOG, "Delayed sync timer with remaining time {} by {} seconds due to nearby new sync run needed.", timer_remaining_msecs / 1000, -difference_msecs / 1000);
            }
            if nearby_scheduled_sync {
                return Some(id);
            }
        }
        None
    }

    /// `slotScheduleFilesDelayedSync()`, with the discovery's
    /// `_filesNeedingScheduledSync` (file → seconds until the lock expires).
    pub fn schedule_files_delayed_sync(
        &mut self,
        files_needing_scheduled_sync: &HashMap<String, i64>,
    ) {
        self.schedule_files_delayed_sync_at(Instant::now(), files_needing_scheduled_sync);
    }

    fn schedule_files_delayed_sync_at(
        &mut self,
        now: Instant,
        files_needing_scheduled_sync: &HashMap<String, i64>,
    ) {
        if files_needing_scheduled_sync.is_empty() {
            return;
        }
        // The latest sync of the interval bucket is the one that goes through and is used in the timer.
        // By running the sync run as late as possible in the selected interval, we try to strike a
        // balance between updating the needed file in a timely manner while also syncing late enough
        // to cover all the files in the interval bucket.
        let interval_secs = SCHEDULED_SYNC_INTERVAL_SECS;
        let buckets =
            self.group_needed_scheduled_sync_runs(files_needing_scheduled_sync, interval_secs);
        log::debug!(target: LOG, "Active scheduled sync run timers: {}", self.list.len());
        for (_, bucket) in buckets {
            let secs = bucket.scheduled_sync_timer_secs;
            // We want to make sure that this bucket won't schedule a sync near a pre-existing sync run,
            // as we often get, for example, locked file notifications one by one as the user interacts
            // through the web.
            if let Some(nearby) = self.nearby_scheduled_sync_timer(now, secs, interval_secs) {
                self.add_files_to_timer(nearby, &bucket.files);
                log::info!(target: LOG, "Using a nearby scheduled sync run in {secs} seconds for files: {:?}", bucket.files);
                continue;
            }
            log::info!(target: LOG, "Will have a new sync run in {secs} seconds for files: {:?}", bucket.files);
            let id = self.next_id;
            self.next_id += 1;
            let mut timer = Timer {
                files: HashSet::new(),
                interval: Duration::ZERO,
                deadline: None,
            };
            timer.start(now, Duration::from_millis((secs * 1000).max(0) as u64));
            self.timers.insert(id, timer);
            self.add_files_to_timer(id, &bucket.files);
            log::debug!(target: LOG, "automated sync will be fired at {}ms", secs * 1000);
            self.list.push(id);
        }
    }

    fn add_files_to_timer(&mut self, id: u64, files: &[String]) {
        for file in files {
            if let Some(t) = self.timers.get_mut(&id) {
                t.files.insert(file.clone());
            }
            if let Some(old) = self.files_scheduled_for_later_sync.insert(file.clone(), id)
                && old != id
            {
                self.drop_if_unreferenced(old);
            }
        }
    }

    /// Releases a timer no longer referenced by the list or the hash (the
    /// last `QSharedPointer` going away).
    fn drop_if_unreferenced(&mut self, id: u64) {
        if !self.list.contains(&id)
            && !self
                .files_scheduled_for_later_sync
                .values()
                .any(|v| *v == id)
        {
            self.timers.remove(&id);
        }
    }

    /// `slotCleanupScheduledSyncTimers()`.
    pub fn cleanup_scheduled_sync_timers(&mut self) {
        log::debug!(target: LOG, "Beginning scheduled sync timer cleanup.");
        let mut kept = Vec::new();
        let mut erased = Vec::new();
        for id in std::mem::take(&mut self.list) {
            match self.timers.get_mut(&id) {
                Some(timer) if timer.files.is_empty() || !timer.is_active() => {
                    log::info!(target: LOG, "Stopping and erasing an expired/empty scheduled sync run timer.");
                    timer.deadline = None;
                    erased.push(id);
                }
                Some(_) => kept.push(id),
                None => log::info!(target: LOG, "Erasing a null scheduled sync run timer."),
            }
        }
        self.list = kept;
        for id in erased {
            self.drop_if_unreferenced(id);
        }
    }

    /// `slotUnscheduleFilesDelayedSync()`, with the discovery's
    /// `_filesUnscheduleSync`.
    pub fn unschedule_files_delayed_sync(&mut self, files_unschedule_sync: &[String]) {
        if files_unschedule_sync.is_empty() {
            return;
        }
        let now = Instant::now();
        for file in files_unschedule_sync {
            if let Some(id) = self.files_scheduled_for_later_sync.get(file)
                && let Some(timer) = self.timers.get_mut(id)
            {
                timer.files.remove(file);
                log::info!(target: LOG, "Removed {file} from sync run timer elapsing in {}ms, this timer is still running for files: {:?}", timer.remaining_msecs(now), timer.files);
            }
        }
        self.cleanup_scheduled_sync_timers();
    }

    /// Fires the timers whose deadline passed (their `callOnTimeout`
    /// lambda): their files are no longer scheduled and the timers are
    /// cleaned up. Returns true when at least one fired, i.e. when the
    /// engine must `startSync()`.
    pub fn fire_due(&mut self, now: Instant) -> bool {
        let due: Vec<u64> = self
            .list
            .iter()
            .copied()
            .filter(|id| {
                self.timers
                    .get(id)
                    .and_then(|t| t.deadline)
                    .is_some_and(|d| d <= now)
            })
            .collect();
        if due.is_empty() {
            return false;
        }
        for id in due {
            let files: Vec<String> = match self.timers.get_mut(&id) {
                Some(t) => {
                    t.deadline = None;
                    t.files.iter().cloned().collect()
                }
                None => continue,
            };
            log::info!(target: LOG, "Rescanning now that delayed sync run is scheduled for: {files:?}");
            for file in files {
                self.files_scheduled_for_later_sync.remove(&file);
            }
        }
        self.cleanup_scheduled_sync_timers();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(&str, i64)]) -> HashMap<String, i64> {
        items.iter().map(|(f, s)| ((*f).to_owned(), *s)).collect()
    }

    #[test]
    fn derived_files_in_one_minute_share_a_timer_firing_at_the_latest() {
        let now = Instant::now();
        let mut t = ScheduledSyncTimers::new();
        t.schedule_files_delayed_sync_at(now, &map(&[("a", 70), ("b", 100), ("c", 200)]));
        // buckets 1 (a, b → 100 s) and 3 (c → 200 s)
        assert_eq!(t.len(), 2);
        assert_eq!(t.next_deadline(), Some(now + Duration::from_secs(100)));
        assert!(!t.fire_due(now + Duration::from_secs(99)));
        assert!(t.fire_due(now + Duration::from_secs(100)));
        assert!(!t.is_scheduled("a") && !t.is_scheduled("b") && t.is_scheduled("c"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn derived_already_scheduled_file_is_not_rescheduled_unless_extended() {
        let now = Instant::now();
        let mut t = ScheduledSyncTimers::new();
        t.schedule_files_delayed_sync_at(now, &map(&[("a", 100)]));
        t.schedule_files_delayed_sync_at(now, &map(&[("a", 90)]));
        assert_eq!(t.len(), 1);
        // a nearby new bucket reuses (and delays) the existing timer
        t.schedule_files_delayed_sync_at(now, &map(&[("a", 110)]));
        assert_eq!(t.len(), 1);
        assert_eq!(t.next_deadline(), Some(now + Duration::from_secs(110)));
    }

    #[test]
    fn derived_unscheduling_the_last_file_erases_the_timer() {
        let now = Instant::now();
        let mut t = ScheduledSyncTimers::new();
        t.schedule_files_delayed_sync_at(now, &map(&[("a", 100)]));
        t.unschedule_files_delayed_sync(&["a".to_owned()]);
        assert!(t.is_empty());
        assert_eq!(t.next_deadline(), None);
    }
}
