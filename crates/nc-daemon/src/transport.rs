// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The transport of the control endpoint: a Unix domain socket on Unix, a
//! named pipe on Windows (`interprocess` local sockets). The endpoint is
//! named by a [`Path`] everywhere: a socket file on Unix, a pipe name
//! (`\\.\pipe\ncsyncd-<user>-<hash>`, see [`crate::guard::default_endpoint`])
//! on Windows.

use std::io;
use std::path::Path;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{ListenerOptions, Name};

/// The streams the server side reads and writes.
pub type Stream = interprocess::local_socket::tokio::Stream;

#[cfg(unix)]
fn name_of(path: &Path) -> io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName as _};
    path.to_path_buf().to_fs_name::<GenericFilePath>()
}

/// The pipe name: what follows `\\.\pipe\`, or the path with the characters
/// a pipe name cannot hold replaced.
#[cfg(windows)]
fn pipe_name(path: &Path) -> String {
    let text = path.to_string_lossy();
    let name = text
        .strip_prefix(r"\\.\pipe\")
        .map_or_else(|| text.replace(['\\', '/', ':'], "_"), str::to_owned);
    name
}

#[cfg(windows)]
fn name_of(path: &Path) -> io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName as _};
    pipe_name(path).to_ns_name::<GenericNamespaced>()
}

/// Whether something accepts connections on `path`.
pub fn is_listening(path: &Path) -> bool {
    connect(path).is_ok()
}

/// Binds the endpoint. On Unix a stale socket file of a dead process is
/// replaced; an endpoint somebody listens on is `AddrInUse` on every OS.
pub fn bind(path: &Path) -> io::Result<Listener> {
    if is_listening(path) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another ncsyncd is listening on {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        // A stale socket of a previous run.
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    let listener = ListenerOptions::new()
        .name(name_of(path)?)
        .reclaim_name(false)
        .create_tokio()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(Listener(listener))
}

/// The listening side.
pub struct Listener(interprocess::local_socket::tokio::Listener);

impl Listener {
    pub async fn accept(&self) -> io::Result<Stream> {
        self.0.accept().await
    }
}

/// A blocking connection (for `ncsync` and the engine client), with a read
/// timeout.
#[cfg(unix)]
pub type SyncStream = std::os::unix::net::UnixStream;

/// Connects to the endpoint.
#[cfg(unix)]
pub fn connect(path: &Path) -> io::Result<SyncStream> {
    std::os::unix::net::UnixStream::connect(path)
}

/// A blocking connection with a read timeout: a named pipe has none, so a
/// helper thread reads and hands the bytes over a channel.
#[cfg(windows)]
pub struct SyncStream {
    inner: std::sync::Arc<interprocess::local_socket::Stream>,
    chunks: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: std::collections::VecDeque<u8>,
    timeout: Option<std::time::Duration>,
}

#[cfg(windows)]
impl SyncStream {
    fn new(stream: interprocess::local_socket::Stream) -> Self {
        let inner = std::sync::Arc::new(stream);
        let (tx, chunks) = std::sync::mpsc::channel();
        let reader = inner.clone();
        // Ends with the connection (an empty chunk is the end of the stream).
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = vec![0u8; 8192];
            loop {
                match (&*reader).read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Ok(Vec::new()));
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        });
        Self {
            inner,
            chunks,
            pending: std::collections::VecDeque::new(),
            timeout: None,
        }
    }

    /// `UnixStream::set_read_timeout`.
    pub fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.timeout = timeout;
        Ok(())
    }
}

#[cfg(windows)]
impl io::Read for SyncStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let chunk = match self.timeout {
                None => self
                    .chunks
                    .recv()
                    .map_err(|_| io::ErrorKind::BrokenPipe.into()),
                Some(t) => self.chunks.recv_timeout(t).map_err(|e| match e {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        io::Error::new(io::ErrorKind::TimedOut, "read timed out")
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        io::ErrorKind::BrokenPipe.into()
                    }
                }),
            }??;
            if chunk.is_empty() {
                return Ok(0);
            }
            self.pending.extend(chunk);
        }
        let n = buf.len().min(self.pending.len());
        for (slot, byte) in buf.iter_mut().zip(self.pending.drain(..n)) {
            *slot = byte;
        }
        Ok(n)
    }
}

#[cfg(windows)]
impl io::Write for SyncStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use std::io::Write as _;
        (&*self.inner).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        use std::io::Write as _;
        (&*self.inner).flush()
    }
}

/// Connects to the endpoint.
#[cfg(windows)]
pub fn connect(path: &Path) -> io::Result<SyncStream> {
    use interprocess::local_socket::Stream as Sync;
    use interprocess::local_socket::traits::Stream as _;
    // A pipe whose instances are all in use (the server is between two
    // `accept`s) answers ERROR_PIPE_BUSY: wait a moment, like
    // `WaitNamedPipe`.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let inner = loop {
        match Sync::connect(name_of(path)?) {
            Err(e) if e.raw_os_error() == Some(231) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => break other?,
        }
    };
    Ok(SyncStream::new(inner))
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn pipe_names() {
        assert_eq!(
            super::pipe_name(std::path::Path::new(r"\\.\pipe\ncsyncd-u-ab")),
            "ncsyncd-u-ab"
        );
        assert_eq!(
            super::pipe_name(std::path::Path::new(r"C:\t\x.sock")),
            "C__t_x.sock"
        );
    }
}
