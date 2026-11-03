// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

use std::os::fd::AsRawFd;

use tokio::io::unix::AsyncFd;

/// Registers a file descriptor with tokio's reactor, the `QSocketNotifier`
/// of upstream's `FolderWatcherPrivate`.
pub fn register_async_fd<T: AsRawFd>(fd: T) -> std::io::Result<AsyncFd<T>> {
    // SAFETY: the caller hands over the owner of the descriptor: it stays
    // open, and is the one `as_raw_fd()` returns, until the `AsyncFd` drops
    // `fd`.
    Ok(unsafe { AsyncFd::register(fd) }?)
}
