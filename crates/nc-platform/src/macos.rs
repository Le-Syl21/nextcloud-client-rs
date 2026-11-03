// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The macOS directory watcher: a port of upstream's
//! `src/gui/folderwatcher_mac.cpp` (nextcloud/desktop v34.0.5) on the
//! FSEvents API (`objc2-core-services`).

use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFArray, CFRetained, CFString};
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamEventFlags,
    FSEventStreamEventId, FSEventStreamInvalidate, FSEventStreamRef, FSEventStreamRelease,
    FSEventStreamSetDispatchQueue, FSEventStreamStart, FSEventStreamStop,
    kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagIgnoreSelf,
    kFSEventStreamEventFlagItemCloned, kFSEventStreamEventFlagItemCreated,
    kFSEventStreamEventFlagItemInodeMetaMod, kFSEventStreamEventFlagItemModified,
    kFSEventStreamEventFlagItemRemoved, kFSEventStreamEventFlagItemRenamed,
    kFSEventStreamEventFlagKernelDropped, kFSEventStreamEventFlagMustScanSubDirs,
    kFSEventStreamEventFlagUserDropped,
};
use unicode_normalization::UnicodeNormalization as _;

use crate::DirEvent;

/// `kFSEventStreamEventIdSinceNow`.
const SINCE_NOW: FSEventStreamEventId = u64::MAX;

/// What the callback needs.
struct Context {
    root: String,
    sink: Box<dyn Fn(DirEvent) + Send + Sync>,
}

/// `FolderWatcherPrivate`: one FSEvents stream on the root. Dropping it
/// stops the stream.
pub struct DirWatcher {
    stream: FSEventStreamRef,
    _queue: DispatchRetained<DispatchQueue>,
    context: *mut Context,
}

// SAFETY: the stream is only touched by this owner (start, stop on drop); the
// callback runs on the private dispatch queue and only reads the `Context`,
// whose sink is `Send + Sync`.
unsafe impl Send for DirWatcher {}

impl DirWatcher {
    /// Starts watching `root` (a `/`-separated absolute path).
    pub fn start(
        root: &str,
        sink: impl Fn(DirEvent) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let folder = CFString::from_str(root);
        let paths: CFRetained<CFArray<CFString>> = CFArray::from_retained_objects(&[folder]);
        let context = Box::into_raw(Box::new(Context {
            root: root.trim_end_matches('/').to_owned(),
            sink: Box::new(sink),
        }));
        let mut stream_context = FSEventStreamContext {
            version: 0,
            info: context.cast(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: `callback` matches FSEventStreamCallback; `context` stays
        // allocated until `drop` (after the stream is invalidated); the
        // path array is a valid CFArray of CFString. The events are plain C
        // strings (no `UseCFTypes`).
        let stream = unsafe {
            FSEventStreamCreate(
                None,
                Some(callback),
                &mut stream_context,
                paths.as_opaque(),
                SINCE_NOW,
                0.0, // latency
                kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagIgnoreSelf,
            )
        };
        if stream.is_null() {
            // SAFETY: created above, never used by a stream.
            drop(unsafe { Box::from_raw(context) });
            return Err(std::io::Error::other("FSEventStreamCreate failed"));
        }
        let queue = DispatchQueue::new("nc-client.fsevents", None);
        // SAFETY: `stream` is valid; the queue is retained by `Self`.
        unsafe { FSEventStreamSetDispatchQueue(stream, Some(&queue)) };
        // SAFETY: `stream` is valid and has a queue.
        if !unsafe { FSEventStreamStart(stream) } {
            // SAFETY: valid stream, not started: invalidate and release.
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                drop(Box::from_raw(context));
            }
            return Err(std::io::Error::other("FSEventStreamStart failed"));
        }
        // SAFETY: `context` is alive.
        unsafe { ((*context).sink)(DirEvent::Ready) };
        Ok(Self {
            stream,
            _queue: queue,
            context,
        })
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        // SAFETY: the stream was created and started by `start`; after it is
        // stopped and invalidated no callback runs, so the context can go.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.context));
        }
    }
}

/// The FSEvents callback.
unsafe extern "C-unwind" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: NonNull<c_void>,
    event_flags: NonNull<FSEventStreamEventFlags>,
    _event_ids: NonNull<FSEventStreamEventId>,
) {
    // SAFETY: `info` is the `Context` registered in `start`, alive until the
    // stream is invalidated.
    let context = unsafe { &*info.cast::<Context>() };
    // SAFETY: FSEvents passes `num_events` flags and, without UseCFTypes,
    // `num_events` NUL-terminated C strings.
    let flags = unsafe { std::slice::from_raw_parts(event_flags.as_ptr(), num_events) };
    // SAFETY: see above.
    let paths = unsafe {
        std::slice::from_raw_parts(event_paths.as_ptr().cast::<*const c_char>(), num_events)
    };

    // These flags indicate that either due to system error or for unknown
    // reasons, the entire directory structure must be rescanned (including
    // files)
    let full_scan = kFSEventStreamEventFlagMustScanSubDirs
        | kFSEventStreamEventFlagKernelDropped
        | kFSEventStreamEventFlagUserDropped;
    if flags.iter().any(|f| f & full_scan != 0) {
        // notifyAll(): everything below the root changed
        (context.sink)(DirEvent::LostChanges);
        (context.sink)(DirEvent::Changed(context.root.clone()));
        return;
    }

    // We ignore other flags, e.g. for owner change, xattr change, Finder
    // label change etc
    let interesting = kFSEventStreamEventFlagItemCreated // for new folder/file
        | kFSEventStreamEventFlagItemRemoved // for rm
        | kFSEventStreamEventFlagItemInodeMetaMod // for mtime change
        | kFSEventStreamEventFlagItemRenamed // also coming for moves to trash in finder
        | kFSEventStreamEventFlagItemModified // for content change
        | kFSEventStreamEventFlagItemCloned; // for cloned items (since 10.13)
    for (flag, path) in flags.iter().zip(paths) {
        // SAFETY: a valid C string for the duration of the callback.
        let raw = unsafe { CStr::from_ptr(*path) }.to_string_lossy();
        let normalized: String = raw.nfc().collect();
        if flag & interesting == 0 {
            log::debug!(target: "nextcloud.gui.folderwatcher", "Ignoring non-content changes for {normalized} {flag}");
            continue;
        }
        (context.sink)(DirEvent::Changed(normalized));
    }
}
