// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream `test/testsyncfilestatustracker.cpp` (nextcloud/desktop v34.0.5).
//
// Upstream pauses the sync with `execUntilBeforePropagation()` /
// `execUntilItemCompleted(path)` and checks the statuses in between. The
// port's sync is one future borrowing the engine, so those checks run in
// engine callbacks at the same points instead: `aboutToPropagate` (after the
// tracker handled it, as the tracker's connection comes first) and
// `itemCompleted` of the given path. Upstream's pause point is also after
// the `started` signal; the only status that signal pushes is the root's,
// which `aboutToPropagate` already pushed with the same value in every
// test here.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use nc_sync::sync_file_status::{SyncFileStatus, SyncFileStatusTag};
use nc_sync::sync_file_status_tracker::SyncFileStatusTracker;
use nc_testutils::{EtagsAction, FakeFolder, FileInfo, FileModifier};

use SyncFileStatusTag::*;

fn s(tag: SyncFileStatusTag) -> SyncFileStatus {
    SyncFileStatus::new(tag)
}

/// `StatusPushSpy`: records `fileStatusChanged(systemFileName, status)`.
#[derive(Clone)]
struct StatusPushSpy {
    local_path: String,
    records: Rc<RefCell<Vec<(String, SyncFileStatus)>>>,
}

/// `QFileInfo::operator==`: the same (clean) path, else the same canonical
/// path, looked up now. A path that does not exist has an empty canonical
/// path, so two missing files compare equal (upstream's
/// `statusOf("A/a1") == statusOf("A/a1notexist")` relies on it).
fn same_file(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    if a == b {
        return true;
    }
    std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok()
}

impl StatusPushSpy {
    fn new(fake_folder: &mut FakeFolder) -> Self {
        let records: Rc<RefCell<Vec<(String, SyncFileStatus)>>> = Rc::default();
        let r = records.clone();
        fake_folder
            .sync_engine()
            .sync_file_status_tracker()
            .borrow_mut()
            .connect_file_status_changed(Box::new(move |path, status| {
                r.borrow_mut().push((path.to_owned(), status));
            }));
        Self {
            local_path: fake_folder.local_path(),
            records,
        }
    }

    fn file(&self, relative_path: &str) -> String {
        format!("{}{relative_path}", self.local_path)
    }

    fn status_of(&self, relative_path: &str) -> SyncFileStatus {
        let file = self.file(relative_path);
        // Start from the end to get the latest status
        self.records
            .borrow()
            .iter()
            .rev()
            .find(|(p, _)| same_file(p, &file))
            .map(|(_, st)| *st)
            .unwrap_or_default()
    }

    fn status_emitted_before(&self, first_path: &str, second_path: &str) -> bool {
        let first_file = self.file(first_path);
        let second_file = self.file(second_path);
        let records = self.records.borrow();
        // Start from the end to get the latest status
        let mut i = records.len();
        while i > 0 {
            i -= 1;
            if same_file(&records[i].0, &second_file) {
                break;
            } else if same_file(&records[i].0, &first_file) {
                return false;
            }
        }
        while i > 0 {
            i -= 1;
            if same_file(&records[i].0, &first_file) {
                return true;
            }
        }
        false
    }

    fn clear(&self) {
        self.records.borrow_mut().clear();
    }
}

/// What the checks need: the spy and the tracker (pulls).
#[derive(Clone)]
struct Ctx {
    spy: StatusPushSpy,
    tracker: Rc<RefCell<SyncFileStatusTracker>>,
}

impl Ctx {
    fn new(fake_folder: &mut FakeFolder) -> Self {
        let spy = StatusPushSpy::new(fake_folder);
        let tracker = fake_folder.sync_engine().sync_file_status_tracker();
        Self { spy, tracker }
    }

    fn pull(&self, relative_path: &str) -> SyncFileStatus {
        self.tracker.borrow().file_status(relative_path)
    }

    /// `verifyThatPushMatchesPull`: every entry of the folder (hidden
    /// entries excluded, like `QDir::AllEntries`) whose status was pushed
    /// has that status when pulled.
    fn verify_that_push_matches_pull(&self) {
        let root = self.spy.local_path.clone();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let full = Path::new(&dir).join(&name).to_string_lossy().into_owned();
                let file_path = full[root.len()..].to_owned();
                let pushed_status = self.spy.status_of(&file_path);
                if pushed_status != SyncFileStatus::default() {
                    assert_eq!(self.pull(&file_path), pushed_status, "{file_path}");
                }
                if entry.file_type().unwrap().is_dir() {
                    stack.push(full);
                }
            }
        }
    }
}

/// `scheduleSync(); execUntilBeforePropagation();` then `before`, then
/// `execUntilFinished()`.
fn sync_with_check_before_propagation(
    fake_folder: &mut FakeFolder,
    mut before: impl FnMut() + 'static,
) -> bool {
    let ran = Rc::new(Cell::new(false));
    let r = ran.clone();
    fake_folder.sync_engine().callbacks().about_to_propagate = Some(Box::new(move |_| {
        before();
        r.set(true);
    }));
    let result = fake_folder.sync_once();
    fake_folder.sync_engine().callbacks().about_to_propagate = None;
    assert!(ran.get(), "the sync did not reach the propagation");
    result
}

/// Also runs `at_item` once `item_path` completed (`execUntilItemCompleted`).
fn sync_with_checks(
    fake_folder: &mut FakeFolder,
    before: impl FnMut() + 'static,
    item_path: &'static str,
    mut at_item: impl FnMut() + 'static,
) -> bool {
    let ran = Rc::new(Cell::new(false));
    let r = ran.clone();
    fake_folder.sync_engine().callbacks().item_completed = Some(Box::new(move |item, _| {
        if !r.get() && item.borrow().destination() == item_path {
            r.set(true);
            at_item();
        }
    }));
    let result = sync_with_check_before_propagation(fake_folder, before);
    fake_folder.sync_engine().callbacks().item_completed = None;
    assert!(ran.get(), "{item_path} did not complete");
    result
}

// initTestCase: logger and QStandardPaths test mode, n/a.

#[test]
fn parents_get_sync_status_upload_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.local_modifier().append_byte("B/b1");
    fake_folder.remote_modifier().append_byte("C/c1");
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        assert_eq!(c.spy.status_of("B/b1"), s(StatusSync));
        assert_eq!(c.spy.status_of("C"), s(StatusSync));
        assert_eq!(c.spy.status_of("C/c1"), s(StatusSync));
        assert_eq!(c.pull("A"), s(StatusUpToDate));
        assert_eq!(c.pull("A/a1"), s(StatusUpToDate));
        assert_eq!(c.pull("B/b2"), s(StatusUpToDate));
        assert_eq!(c.pull("C/c2"), s(StatusUpToDate));
        c.spy.clear();
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B/b1"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C/c1"), s(StatusUpToDate));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn parents_get_sync_status_new_file_upload_download() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.local_modifier().insert("B/b0", 64, b'W');
    fake_folder.remote_modifier().insert("C/c0", 64, b'W');
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        assert_eq!(c.spy.status_of("B/b0"), s(StatusSync));
        assert_eq!(c.spy.status_of("C"), s(StatusSync));
        assert_eq!(c.spy.status_of("C/c0"), s(StatusSync));
        assert_eq!(c.pull("A"), s(StatusUpToDate));
        assert_eq!(c.pull("A/a1"), s(StatusUpToDate));
        assert_eq!(c.pull("B/b1"), s(StatusUpToDate));
        assert_eq!(c.pull("C/c1"), s(StatusUpToDate));
        c.spy.clear();
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B/b0"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C/c0"), s(StatusUpToDate));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

fn parents_get_sync_status_new_dir(upload: bool) {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    if upload {
        fake_folder.local_modifier().mkdir("D");
        fake_folder.local_modifier().insert("D/d0", 64, b'W');
    } else {
        fake_folder.remote_modifier().mkdir("D");
        fake_folder.remote_modifier().insert("D/d0", 64, b'W');
    }
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    let c2 = ctx.clone();
    sync_with_checks(
        &mut fake_folder,
        move || {
            c.verify_that_push_matches_pull();
            assert_eq!(c.spy.status_of(""), s(StatusSync));
            assert_eq!(c.spy.status_of("D"), s(StatusSync));
            assert_eq!(c.spy.status_of("D/d0"), s(StatusSync));
        },
        "D",
        move || {
            c2.verify_that_push_matches_pull();
            assert_eq!(c2.spy.status_of(""), s(StatusSync));
            assert_eq!(c2.spy.status_of("D"), s(StatusSync));
            assert_eq!(c2.spy.status_of("D/d0"), s(StatusSync));
        },
    );

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("D"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("D/d0"), s(StatusUpToDate));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn parents_get_sync_status_new_dir_download() {
    parents_get_sync_status_new_dir(false);
}

#[test]
fn parents_get_sync_status_new_dir_upload() {
    parents_get_sync_status_new_dir(true);
}

#[test]
fn parents_get_sync_status_delete_up_down() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.remote_modifier().remove("B/b1");
    fake_folder.local_modifier().remove("C/c1");
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        // Discovered as remotely removed, pending for local removal.
        assert_eq!(c.spy.status_of("B/b1"), s(StatusSync));
        assert_eq!(c.spy.status_of("C"), s(StatusSync));
        assert_eq!(c.pull("A"), s(StatusUpToDate));
        assert_eq!(c.pull("B/b2"), s(StatusUpToDate));
        assert_eq!(c.pull("C/c2"), s(StatusUpToDate));
        c.spy.clear();
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C"), s(StatusUpToDate));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn warning_status_for_excluded_file() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder
        .sync_engine()
        .excluded_files()
        .add_manual_exclude("A/a1");
    fake_folder
        .sync_engine()
        .excluded_files()
        .add_manual_exclude("B");
    fake_folder.local_modifier().append_byte("A/a1");
    fake_folder.local_modifier().append_byte("B/b1");
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of("A/a1"), s(StatusExcluded));
        assert_eq!(c.spy.status_of("B"), s(StatusExcluded));
        // QEXPECT_FAIL: csync will stop at ignored directories without traversing children, so we don't currently push the status for newly ignored children of an ignored directory.
        assert_ne!(c.spy.status_of("B/b1"), s(StatusExcluded));
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusExcluded));
    assert_eq!(ctx.spy.status_of("B"), s(StatusExcluded));
    // QEXPECT_FAIL: csync will stop at ignored directories without traversing children, so we don't currently push the status for newly ignored children of an ignored directory.
    assert_ne!(ctx.spy.status_of("B/b1"), s(StatusExcluded));
    // QEXPECT_FAIL: csync will stop at ignored directories without traversing children, so we don't currently push the status for newly ignored children of an ignored directory.
    assert_ne!(ctx.spy.status_of("B/b2"), s(StatusExcluded));
    assert_eq!(ctx.pull(""), s(StatusUpToDate));
    assert_eq!(ctx.pull("A"), s(StatusUpToDate));
    ctx.spy.clear();

    // Clears the exclude expr above
    fake_folder
        .sync_engine()
        .excluded_files()
        .clear_manual_excludes();
    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("A"), s(StatusSync));
        assert_eq!(c.spy.status_of("A/a1"), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        assert_eq!(c.spy.status_of("B/b1"), s(StatusSync));
        c.spy.clear();
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B/b1"), s(StatusUpToDate));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn warning_status_for_excluded_file_case_preserving() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder
        .sync_engine()
        .excluded_files()
        .add_manual_exclude("B");
    fake_folder.server_error_paths().append("A/a1", 500);
    fake_folder.local_modifier().append_byte("A/a1");

    fake_folder.sync_once();
    let tracker = fake_folder.sync_engine().sync_file_status_tracker();
    let pull = |p: &str| tracker.borrow().file_status(p);
    assert_eq!(pull(""), s(StatusWarning));
    assert_eq!(pull("A"), s(StatusWarning));
    assert_eq!(pull("A/a1"), s(StatusError));
    assert_eq!(pull("B"), s(StatusExcluded));

    // Should still get the status for different casing on macOS and Windows.
    let case_preserving = nc_journal::utility::fs_case_preserving();
    let pick = |tag| s(if case_preserving { tag } else { StatusNone });
    assert_eq!(pull("a"), pick(StatusWarning));
    assert_eq!(pull("A/A1"), pick(StatusError));
    assert_eq!(pull("b"), pick(StatusExcluded));
}

#[test]
fn parents_get_warning_status_for_error() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.server_error_paths().append("A/a1", 500);
    fake_folder.server_error_paths().append("B/b0", 500);
    fake_folder.local_modifier().append_byte("A/a1");
    fake_folder.local_modifier().insert("B/b0", 64, b'W');
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("A"), s(StatusSync));
        assert_eq!(c.spy.status_of("A/a1"), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        assert_eq!(c.spy.status_of("B/b0"), s(StatusSync));
        c.spy.clear();
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("A"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusError));
    assert_eq!(ctx.pull("A/a2"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("B/b0"), s(StatusError));
    ctx.spy.clear();

    // Remove the error and start a second sync, the blacklist should kick in
    fake_folder.server_error_paths().clear();
    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        // A/a1 and B/b0 should be on the black list for the next few seconds
        assert_eq!(c.spy.status_of(""), s(StatusWarning));
        assert_eq!(c.spy.status_of("A"), s(StatusWarning));
        assert_eq!(c.spy.status_of("A/a1"), s(StatusError));
        assert_eq!(c.spy.status_of("B"), s(StatusWarning));
        assert_eq!(c.spy.status_of("B/b0"), s(StatusError));
        c.spy.clear();
    });
    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("A"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusError));
    assert_eq!(ctx.pull("A/a2"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("B/b0"), s(StatusError));
    ctx.spy.clear();

    // Start a third sync, this time together with a real file to sync
    fake_folder.local_modifier().append_byte("C/c1");
    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        // The root should show SYNC even though there is an error underneath,
        // since C/c1 is syncing and the SYNC status has priority.
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("A"), s(StatusWarning));
        assert_eq!(c.spy.status_of("A/a1"), s(StatusError));
        assert_eq!(c.spy.status_of("B"), s(StatusWarning));
        assert_eq!(c.spy.status_of("B/b0"), s(StatusError));
        assert_eq!(c.spy.status_of("C"), s(StatusSync));
        assert_eq!(c.spy.status_of("C/c1"), s(StatusSync));
        c.spy.clear();
    });
    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("A"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusError));
    assert_eq!(ctx.pull("A/a2"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("B/b0"), s(StatusError));
    assert_eq!(ctx.spy.status_of("C"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C/c1"), s(StatusUpToDate));
    ctx.spy.clear();

    // Another sync after clearing the blacklist entry, everything should return to order.
    fake_folder
        .sync_journal()
        .wipe_error_blacklist_entry("A/a1");
    fake_folder
        .sync_journal()
        .wipe_error_blacklist_entry("B/b0");
    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("A"), s(StatusSync));
        assert_eq!(c.spy.status_of("A/a1"), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        assert_eq!(c.spy.status_of("B/b0"), s(StatusSync));
        c.spy.clear();
    });
    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B/b0"), s(StatusUpToDate));

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn parents_get_warning_status_for_error_sibbling_starts_with_path() {
    // A is a parent of A/a1, but A/a is not even if it's a substring of A/a1
    let mut fake_folder = FakeFolder::new(FileInfo::dir(
        "",
        [FileInfo::dir(
            "A",
            [FileInfo::file("a", 4), FileInfo::file("a1", 4)],
        )],
    ));
    fake_folder.server_error_paths().append("A/a1", 500);
    fake_folder.local_modifier().append_byte("A/a1");
    let tracker = fake_folder.sync_engine().sync_file_status_tracker();

    let t = tracker.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        // The SyncFileStatusTraker won't push any status for all of them, test with a pull.
        let pull = |p: &str| t.borrow().file_status(p);
        assert_eq!(pull(""), s(StatusSync));
        assert_eq!(pull("A"), s(StatusSync));
        assert_eq!(pull("A/a1"), s(StatusSync));
        assert_eq!(pull("A/a"), s(StatusUpToDate));
    });

    // We use string matching for paths in the implementation,
    // an error should affect only parents and not every path that starts with the problem path.
    let pull = |p: &str| tracker.borrow().file_status(p);
    assert_eq!(pull(""), s(StatusWarning));
    assert_eq!(pull("A"), s(StatusWarning));
    assert_eq!(pull("A/a1"), s(StatusError));
    assert_eq!(pull("A/a"), s(StatusUpToDate));
}

// Even for status pushes immediately following each other, macOS
// can sometimes have 1s delays between updates, so make sure that
// children are marked as OK before their parents do.
#[test]
fn child_ok_emitted_before_parent() {
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.local_modifier().append_byte("B/b1");
    fake_folder.remote_modifier().append_byte("C/c1");
    let ctx = Ctx::new(&mut fake_folder);

    fake_folder.sync_once();
    ctx.verify_that_push_matches_pull();
    assert!(ctx.spy.status_emitted_before("B/b1", "B"));
    assert!(ctx.spy.status_emitted_before("C/c1", "C"));
    assert!(ctx.spy.status_emitted_before("B", ""));
    assert!(ctx.spy.status_emitted_before("C", ""));
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B/b1"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("C/c1"), s(StatusUpToDate));
}

#[test]
fn shared_status() {
    let mut shared_up_to_date_status = s(StatusUpToDate);
    shared_up_to_date_status.set_shared(true);

    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.remote_modifier().insert("S/s0", 64, b'W');
    fake_folder.remote_modifier().append_byte("S/s1");
    fake_folder.remote_modifier().insert("B/b3", 64, b'W');
    fake_folder
        .remote_modifier()
        .find_mut("B/b3")
        .unwrap()
        .extra_dav_properties =
        "<oc:share-types><oc:share-type>0</oc:share-type></oc:share-types>".to_owned();
    fake_folder
        .remote_modifier()
        .find_mut("A/a1")
        .unwrap()
        .is_shared = true; // becomes shared
    fake_folder
        .remote_modifier()
        .find_with("A", EtagsAction::Invalidate); // change the etags of the parent

    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        // We don't care about the shared flag for the sync status,
        // Mac and Windows won't show it and we can't know it for new files.
        assert_eq!(c.spy.status_of("S").tag(), StatusSync);
        assert_eq!(c.spy.status_of("S/s0").tag(), StatusSync);
        assert_eq!(c.spy.status_of("S/s1").tag(), StatusSync);
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("S"), shared_up_to_date_status);
    // QEXPECT_FAIL: We currently only know if a new file is shared on the second sync, after a PROPFIND.
    assert_ne!(ctx.spy.status_of("S/s0"), shared_up_to_date_status);
    assert_eq!(ctx.spy.status_of("S/s1"), shared_up_to_date_status);
    assert!(!ctx.spy.status_of("B/b1").shared());
    assert_eq!(ctx.spy.status_of("B/b3"), shared_up_to_date_status);
    assert_eq!(ctx.spy.status_of("A/a1"), shared_up_to_date_status);

    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
}

#[test]
fn rename_error() {
    // when rename has failed - the old file name must be restored
    let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());
    fake_folder.server_error_paths().append("A/a1", 500);
    fake_folder.local_modifier().rename("A/a1", "A/a1m");
    fake_folder.local_modifier().rename("B/b1", "B/b1m");
    let ctx = Ctx::new(&mut fake_folder);

    let c = ctx.clone();
    sync_with_check_before_propagation(&mut fake_folder, move || {
        c.verify_that_push_matches_pull();

        assert_eq!(c.spy.status_of("A/a1m"), s(StatusSync));
        assert_eq!(c.spy.status_of("A/a1"), c.spy.status_of("A/a1notexist"));
        assert_eq!(c.spy.status_of("A"), s(StatusSync));
        assert_eq!(c.spy.status_of(""), s(StatusSync));
        assert_eq!(c.spy.status_of("B"), s(StatusSync));
        assert_eq!(c.spy.status_of("B/b1m"), s(StatusSync));
    });

    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of("A/a1m"), s(StatusError));
    assert_eq!(ctx.spy.status_of("A/a1"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A"), s(StatusWarning));
    assert_eq!(ctx.spy.status_of(""), s(StatusWarning));
    assert_eq!(ctx.spy.status_of("B"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B/b1m"), s(StatusUpToDate));
    ctx.spy.clear();

    assert!(fake_folder.sync_once());
    ctx.verify_that_push_matches_pull();
    ctx.spy.clear();
    assert!(fake_folder.sync_once());
    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of("A/a1m"), s(StatusNone));
    assert_eq!(ctx.spy.status_of("A/a1"), ctx.spy.status_of("A/a1notexist"));
    assert_eq!(ctx.spy.status_of("A"), s(StatusNone));
    assert_eq!(ctx.spy.status_of(""), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("B"), s(StatusNone));
    assert_eq!(ctx.spy.status_of("B/b1m"), s(StatusNone));
    ctx.spy.clear();
}

#[test]
fn silently_excluded_files_removed_from_exclude() {
    let mut fake_folder = FakeFolder::new(FileInfo::default());
    fake_folder.local_modifier().mkdir("A");
    fake_folder.local_modifier().mkdir("A/photos");
    fake_folder
        .local_modifier()
        .insert("A/photos/image.png", 64, b'W');
    fake_folder
        .local_modifier()
        .insert("A/photos/image1.png", 64, b'W');
    fake_folder
        .local_modifier()
        .insert("A/photos/image2.png", 64, b'W');
    let ctx = Ctx::new(&mut fake_folder);

    fake_folder.sync_once();
    ctx.verify_that_push_matches_pull();
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(ctx.spy.status_of("A/photos/image.png"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A/photos/image1.png"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A/photos/image2.png"), s(StatusUpToDate));
    ctx.spy.clear();

    // add ignore pattern for .png files and Allow to Delete
    fake_folder
        .sync_engine()
        .excluded_files()
        .add_manual_exclude("]*.png");

    // sync again and make sure .png files are ignored
    fake_folder.sync_once();
    ctx.verify_that_push_matches_pull();
    assert_eq!(ctx.spy.status_of("A/photos/image.png"), s(StatusExcluded));
    assert_eq!(ctx.spy.status_of("A/photos/image1.png"), s(StatusExcluded));
    assert_eq!(ctx.spy.status_of("A/photos/image2.png"), s(StatusExcluded));
    ctx.spy.clear();

    // remove exclude for .png files
    fake_folder
        .sync_engine()
        .excluded_files()
        .clear_manual_excludes();
    fake_folder
        .sync_engine()
        .excluded_files()
        .reload_exclude_files();

    // make sure the status is again correct
    fake_folder.sync_once();
    ctx.verify_that_push_matches_pull();
    assert_eq!(
        fake_folder.current_local_state(),
        fake_folder.current_remote_state()
    );
    assert_eq!(ctx.spy.status_of("A/photos/image.png"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A/photos/image1.png"), s(StatusUpToDate));
    assert_eq!(ctx.spy.status_of("A/photos/image2.png"), s(StatusUpToDate));
    ctx.spy.clear();
}
