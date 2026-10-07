// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the touched-files bookkeeping of upstream
// `src/libsync/syncengine.{h,cpp}` (`slotAddTouchedFile`,
// `slotClearTouchedFiles`, `wasFileTouched`, `_clearTouchedFilesTimer`)
// (nextcloud/desktop v34.0.5).

//! The files the sync engine touched recently, so that the folder watcher
//! can tell our own changes from the user's (`SyncEngine::wasFileTouched`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// When the client touches a file, block change notifications for this
/// duration (`s_touchedFilesMaxAgeMs`).
///
/// This seems like a significant delay but it is quite typical for a
/// notification to arrive after the file was touched: some time could pass
/// between the client recording that a file will be touched and its
/// filesystem operation finishing, and then the notification has to be
/// delivered.
pub const TOUCHED_FILES_MAX_AGE: Duration = Duration::from_secs(3);

/// `_clearTouchedFilesTimer` interval: the list is wiped this long after a
/// sync finished.
pub const CLEAR_TOUCHED_FILES_DELAY: Duration = Duration::from_secs(30);

/// `_touchedFiles` with `_clearTouchedFilesTimer`.
///
/// The timer is modelled as a deadline checked lazily: the list is
/// cleared the first time it is used after the deadline, which is not
/// observable from the outside.
#[derive(Debug, Default)]
pub struct TouchedFiles {
    /// Oldest first (`QMultiMap<QElapsedTimer, QString>`).
    entries: VecDeque<(Instant, String)>,
    clear_deadline: Option<Instant>,
}

impl TouchedFiles {
    pub fn new() -> Self {
        Self::default()
    }

    fn clear_if_timer_expired(&mut self, now: Instant) {
        if self.clear_deadline.is_some_and(|d| now >= d) {
            // slotClearTouchedFiles
            self.entries.clear();
            self.clear_deadline = None;
        }
    }

    /// `slotAddTouchedFile(fn)`.
    pub fn add(&mut self, file_name: &str) {
        let now = Instant::now();
        self.clear_if_timer_expired(now);
        let file = clean_path(file_name);
        // Iterate from the oldest and remove anything older than 15 seconds.
        while let Some((first, _)) = self.entries.front() {
            if now.duration_since(*first) <= TOUCHED_FILES_MAX_AGE {
                // We found the first path younger than the maximum age, keep the rest.
                break;
            }
            self.entries.pop_front();
        }
        self.entries.push_back((now, file));
    }

    /// `slotClearTouchedFiles()`.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// `wasFileTouched(fn)`: whether the file was touched by a job less than
    /// [`TOUCHED_FILES_MAX_AGE`] ago.
    pub fn was_file_touched(&mut self, file_name: &str) -> bool {
        let now = Instant::now();
        self.clear_if_timer_expired(now);
        // Start from the end (most recent) and look for our path. Check the time just in case.
        for (time, file) in self.entries.iter().rev() {
            if file == file_name {
                return now.duration_since(*time) <= TOUCHED_FILES_MAX_AGE;
            }
        }
        false
    }

    /// `_clearTouchedFilesTimer.stop()` (at the start of a sync).
    pub fn stop_clear_timer(&mut self) {
        self.clear_deadline = None;
    }

    /// `_clearTouchedFilesTimer.start()` (when a sync is finalized).
    pub fn start_clear_timer(&mut self) {
        self.clear_deadline = Some(Instant::now() + CLEAR_TOUCHED_FILES_DELAY);
    }
}

/// `QDir::cleanPath`: collapses `//`, resolves `.` and `..`, drops a
/// trailing '/' (except for the root).
pub fn clean_path(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        if p.is_empty() {
            String::new()
        } else {
            ".".to_owned()
        }
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_touched_files_are_found_by_clean_path() {
        let mut t = TouchedFiles::new();
        t.add("/a//b/./c");
        assert!(t.was_file_touched("/a/b/c"));
        assert!(!t.was_file_touched("/a/b"));
        t.clear();
        assert!(!t.was_file_touched("/a/b/c"));
    }

    #[test]
    fn derived_clean_path() {
        assert_eq!(clean_path("/a/../b/"), "/b");
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path("a/./b//c"), "a/b/c");
        assert_eq!(clean_path("../x"), "../x");
    }
}
