// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `QTimer` on the daemon's event loop.
//!
//! Upstream's daemon logic is a set of `QTimer`s whose `timeout()` signals
//! are delivered by the Qt event loop. Here a timer is a local task that
//! sleeps and then posts an event to the daemon's queue; the event carries
//! the timer's generation so that a timeout posted just before the timer
//! was stopped or restarted is recognised as stale and dropped, which is
//! what `QTimer::stop()` guarantees.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// A single-shot or repeating timer posting `T` events.
#[derive(Debug)]
pub struct Timer {
    interval: Duration,
    single_shot: bool,
    generation: u64,
    deadline: Option<Instant>,
    task: Option<JoinHandle<()>>,
}

impl Default for Timer {
    fn default() -> Self {
        Self::new(Duration::ZERO, true)
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl Timer {
    pub fn new(interval: Duration, single_shot: bool) -> Self {
        Self {
            interval,
            single_shot,
            generation: 0,
            deadline: None,
            task: None,
        }
    }

    /// `setInterval()`: takes effect at the next `start()` (a running
    /// repeating timer is restarted, like `QTimer`).
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn set_single_shot(&mut self, single_shot: bool) {
        self.single_shot = single_shot;
    }

    /// `isActive()`.
    pub fn is_active(&self) -> bool {
        self.deadline.is_some()
    }

    /// `remainingTime()`.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// The generation a timeout event must carry to be current.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// `start()`: (re)starts the timer; every timeout posts
    /// `make_event(generation)` to `tx`.
    pub fn start<T: 'static>(
        &mut self,
        tx: &UnboundedSender<T>,
        make_event: impl Fn(u64) -> T + 'static,
    ) {
        self.stop();
        self.generation += 1;
        let generation = self.generation;
        let interval = self.interval;
        let single_shot = self.single_shot;
        let start = Instant::now();
        self.deadline = Some(start + interval);
        let tx = tx.clone();
        self.task = Some(tokio::task::spawn_local(async move {
            let mut next = start + interval;
            loop {
                tokio::time::sleep_until(next).await;
                if tx.send(make_event(generation)).is_err() || single_shot {
                    return;
                }
                // A zero interval would spin: Qt runs such a timer once per
                // event loop iteration, yield instead.
                if interval.is_zero() {
                    tokio::task::yield_now().await;
                }
                next += interval;
            }
        }));
    }

    /// `start(msec)`: sets the interval and starts.
    pub fn start_with<T: 'static>(
        &mut self,
        interval: Duration,
        tx: &UnboundedSender<T>,
        make_event: impl Fn(u64) -> T + 'static,
    ) {
        self.interval = interval;
        self.start(tx, make_event);
    }

    /// `stop()`.
    pub fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.deadline = None;
        self.generation += 1;
    }

    /// To be called by the event handler with the generation the event
    /// carries: returns whether the timeout is current (not stale). A
    /// single-shot timer becomes inactive; a repeating one moves its
    /// deadline to the next timeout.
    pub fn fired(&mut self, generation: u64) -> bool {
        if generation != self.generation || self.deadline.is_none() {
            return false;
        }
        if self.single_shot {
            self.deadline = None;
            self.task = None;
        } else {
            self.deadline = Some(Instant::now() + self.interval);
        }
        true
    }
}

/// `QTimer::singleShot(delay, ...)`: posts `event` once after `delay`.
/// Cannot be cancelled, like upstream's lambdas bound to a long-lived
/// receiver.
pub fn single_shot<T: 'static>(delay: Duration, tx: &UnboundedSender<T>, event: T) {
    let tx = tx.clone();
    tokio::task::spawn_local(async move {
        tokio::time::sleep(delay).await;
        let _ = tx.send(event);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn derived_stopped_timer_events_are_stale() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let mut t = Timer::new(Duration::from_millis(100), true);
                t.start(&tx, |g| g);
                let g = rx.recv().await.unwrap();
                assert!(t.fired(g));
                assert!(!t.is_active());
                // a restarted timer drops the previous generation
                t.start(&tx, |g| g);
                let old = t.generation() - 1;
                assert!(!t.fired(old));
                t.stop();
                assert!(!t.is_active());
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn derived_repeating_timer_keeps_firing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let mut t = Timer::new(Duration::from_secs(30), false);
                t.start(&tx, |g| g);
                for _ in 0..3 {
                    let g = rx.recv().await.unwrap();
                    assert!(t.fired(g));
                    assert!(t.is_active());
                }
            })
            .await;
    }
}
