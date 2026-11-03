// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/folderwatcher_win.cpp` (nextcloud/desktop
// v34.0.5): `FolderWatcherPrivate`, with the `ReadDirectoryChangesW` thread
// in `nc-platform`.

//! The folder watcher backends other than inotify.
//!
//! * Windows: one recursive `ReadDirectoryChangesW` on the root, on a
//!   thread of its own (`nc_platform::windows::DirWatcher`); its
//!   notifications cross to the single-threaded watcher through a channel,
//!   the way Qt delivers the signals of `WatcherThread` to the GUI thread.
//!   A buffer overflow is `lostChanges`.
//! * macOS: one FSEvents stream on the root (`nc_platform::macos`, port of
//!   `folderwatcher_mac.cpp`), delivered the same way.
//! * Other systems: no watcher. It reports itself unreliable at once, so that the folder
//!   falls back to the periodic full local discovery.

use std::cell::RefCell;
use std::rc::Rc;

use tokio::task::JoinHandle;

use super::Inner;

/// `FolderWatcherPrivate`.
pub(super) struct FolderWatcherPrivate {
    /// `_ready`: the first notification request is armed.
    pub ready: bool,
    #[cfg(windows)]
    _watcher: Option<nc_platform::windows::DirWatcher>,
    #[cfg(target_os = "macos")]
    _watcher: Option<nc_platform::macos::DirWatcher>,
}

impl std::fmt::Debug for FolderWatcherPrivate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderWatcherPrivate")
            .field("ready", &self.ready)
            .finish()
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl FolderWatcherPrivate {
    /// `FolderWatcherPrivate(p, path)`: starts the thread and the task that
    /// forwards its notifications.
    pub(super) fn start(root: &str, inner: &Rc<RefCell<Inner>>) -> (Self, Option<JoinHandle<()>>) {
        use nc_platform::DirEvent;
        #[cfg(target_os = "macos")]
        use nc_platform::macos::DirWatcher;
        #[cfg(windows)]
        use nc_platform::windows::DirWatcher;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DirEvent>();
        let watcher = match DirWatcher::start(root, move |e| {
            let _ = tx.send(e);
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!(target: super::LOG, "Failed to start the folder watcher thread: {e}");
                None
            }
        };
        let weak = Rc::downgrade(inner);
        let reader = tokio::task::spawn_local(async move {
            while let Some(event) = rx.recv().await {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let mut inner = inner.borrow_mut();
                match event {
                    DirEvent::Ready => {
                        if let Some(d) = inner.d.as_mut() {
                            d.ready = true;
                        }
                    }
                    DirEvent::Changed(path) => inner.core.change_detected_path(&path),
                    DirEvent::LostChanges => {
                        inner.core.emit(super::FolderWatcherEvent::LostChanges)
                    }
                }
            }
        });
        (
            Self {
                ready: false,
                _watcher: watcher,
            },
            Some(reader),
        )
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
impl FolderWatcherPrivate {
    /// No folder watcher on this system yet.
    pub(super) fn start(_root: &str, inner: &Rc<RefCell<Inner>>) -> (Self, Option<JoinHandle<()>>) {
        let mut inner = inner.borrow_mut();
        if inner.core.is_reliable {
            inner.core.is_reliable = false;
            inner.core.emit(super::FolderWatcherEvent::BecameUnreliable(
                "There is no folder watcher for this system yet; the folder is checked periodically."
                    .to_owned(),
            ));
        }
        (Self { ready: true }, None)
    }
}
