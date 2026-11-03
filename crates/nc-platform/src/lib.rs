// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! Operating system calls of nextcloud-client-rs.
//!
//! This is the only crate of the workspace allowed to contain `unsafe`
//! (every other crate keeps `unsafe_code = "deny"`). It holds what the
//! standard library and the safe crates do not offer:
//!
//! * [`paths`]: the slash-path helpers of the port (`FileSystem::longWinPath`,
//!   `Utility::isPathWindowsDrivePartitionRoot`, absolute path tests that
//!   know drive letters); pure functions, tested on every OS;
//! * [`process`]: host name and "is this process alive" for the lock file;
//! * `windows`: Win32 file attributes, file index, volume information and
//!   the `ReadDirectoryChangesW` directory watcher (port of
//!   `UP/src/gui/folderwatcher_win.cpp`);
//! * `fd`: registration of a file descriptor with tokio's reactor (Unix).

pub mod paths;
pub mod process;

#[cfg(unix)]
mod fd;
#[cfg(unix)]
pub use fd::register_async_fd;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

/// What a directory watcher backend reports (upstream's `WatcherThread`
/// signals on Windows and the FSEvents callback on macOS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirEvent {
    /// `ready()`: the watcher is armed.
    Ready,
    /// `changed(path)`: an absolute, `/`-separated path.
    Changed(String),
    /// `lostChanges()`: notifications were lost; rescan.
    LostChanges,
}
