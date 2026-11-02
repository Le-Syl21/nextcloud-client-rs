// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The engine as a GUI sees it: one interface ([`EngineClient`]) with two
//! implementations (decision D3 of `docs/gui-plan.md`).
//!
//! * [`InProcess`]: the GUI owns the configuration (it holds the
//!   [`crate::guard::InstanceGuard`]) and runs the engine on a thread of
//!   its own, with the current-thread runtime and `LocalSet` the folder
//!   manager needs (it is `!Send`), like `ncsyncd`'s main thread.
//! * [`Remote`]: `ncsyncd` holds the configuration; the GUI drives it over
//!   its control endpoint ([`crate::control`]).
//!
//! Both take the same [`Request`]s and give the same [`UiEvent`] stream, so
//! the GUI never reaches into the engine by another path. Neither needs an
//! async runtime on the caller's side: answers come on a `oneshot`
//! receiver (`try_recv` from a UI frame, `blocking_recv` from a worker
//! thread, or `.await`), events on a [`Subscription`].

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::control::{Request, Response, error};
use crate::daemon::DaemonHooks;
use crate::event::Event;
use crate::folder_man::FolderMan;
use crate::ui_event::{UI_EVENT_CAPACITY, UiEvent};

/// The answer to a request, once the engine gave it.
pub type PendingResponse = oneshot::Receiver<Response>;

/// The engine, seen from a GUI.
pub trait EngineClient: Send + Sync {
    /// Sends a request; the answer arrives on the returned receiver (an
    /// `{"ok":false,"error":...}` answer when the engine cannot be
    /// reached).
    fn request(&self, request: Request) -> PendingResponse;

    /// Subscribes to the engine's events from now on.
    fn subscribe(&self) -> Subscription;
}

/// Why [`Subscription::try_recv`] has no event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// Nothing new.
    Empty,
    /// The engine stopped (or the connection to it was lost).
    Closed,
}

/// A subscriber of the [`UiEvent`]s. Falling behind gives
/// [`UiEvent::Lagged`]; the end of the engine ends the stream.
pub struct Subscription {
    rx: broadcast::Receiver<UiEvent>,
}

impl Subscription {
    fn map(r: Result<UiEvent, broadcast::error::RecvError>) -> Option<UiEvent> {
        match r {
            Ok(e) => Some(e),
            Err(broadcast::error::RecvError::Lagged(missed)) => Some(UiEvent::Lagged { missed }),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    /// The next event; `None` once the engine is gone.
    pub async fn recv(&mut self) -> Option<UiEvent> {
        Self::map(self.rx.recv().await)
    }

    /// The next event, blocking the thread (not inside an async runtime).
    pub fn blocking_recv(&mut self) -> Option<UiEvent> {
        Self::map(self.rx.blocking_recv())
    }

    /// The next event if there is one.
    pub fn try_recv(&mut self) -> Result<UiEvent, TryRecvError> {
        use broadcast::error::TryRecvError as E;
        match self.rx.try_recv() {
            Ok(e) => Ok(e),
            Err(E::Lagged(missed)) => Ok(UiEvent::Lagged { missed }),
            Err(E::Empty) => Err(TryRecvError::Empty),
            Err(E::Closed) => Err(TryRecvError::Closed),
        }
    }
}

/// What the engine thread receives from other threads (an [`Event`] is not
/// `Send`).
enum Inbound {
    Control(Request, oneshot::Sender<Response>),
    Shutdown,
}

/// What [`InProcess::start`] builds on the engine thread: the folder
/// manager (accounts and folders set up) and the hooks of the loop. The
/// builder may also serve the control endpoint ([`crate::control::serve`]),
/// so that `ncsync` and a second GUI launch reach this engine.
pub type Built = (FolderMan, Box<dyn DaemonHooks>);

/// The engine running in this process, on its own thread.
pub struct InProcess {
    inbound: mpsc::UnboundedSender<Inbound>,
    /// Weak, so the subscriptions end with the engine.
    ui: broadcast::WeakSender<UiEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InProcess {
    /// Starts the engine thread: `build` runs there, inside the `LocalSet`,
    /// with the event queue's sender; then the event loop
    /// ([`crate::daemon::run`]) runs until [`Self::shutdown`].
    pub fn start<F>(build: F) -> Result<Self, String>
    where
        F: FnOnce(&mpsc::UnboundedSender<Event>) -> Result<Built, String> + Send + 'static,
    {
        let (inbound, mut inbound_rx) = mpsc::unbounded_channel::<Inbound>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("nc-engine".to_owned())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("cannot start the event loop: {e}")));
                        return;
                    }
                };
                tokio::task::LocalSet::new().block_on(&rt, async move {
                    let (tx, mut rx) = mpsc::unbounded_channel();
                    let (mut fm, mut hooks) = match build(&tx) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(fm.ui_sender()));
                    let forward = tx.clone();
                    tokio::task::spawn_local(async move {
                        while let Some(i) = inbound_rx.recv().await {
                            let e = match i {
                                Inbound::Control(r, reply) => Event::Control(r, reply),
                                Inbound::Shutdown => Event::Shutdown,
                            };
                            if forward.send(e).is_err() {
                                return;
                            }
                        }
                        // Every handle is gone.
                        let _ = forward.send(Event::Shutdown);
                    });
                    crate::daemon::run(&mut fm, &tx, &mut rx, hooks.as_mut()).await;
                });
            })
            .map_err(|e| format!("cannot start the engine thread: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(ui)) => Ok(Self {
                inbound,
                ui: ui.downgrade(),
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the engine thread stopped while starting".to_owned())
            }
        }
    }

    /// Stops the engine (running syncs get [`crate::daemon::SHUTDOWN_GRACE`]
    /// to abort) and waits for its thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.inbound.send(Inbound::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for InProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

impl EngineClient for InProcess {
    fn request(&self, request: Request) -> PendingResponse {
        let (tx, rx) = oneshot::channel();
        if let Err(mpsc::error::SendError(Inbound::Control(_, tx))) =
            self.inbound.send(Inbound::Control(request, tx))
        {
            let _ = tx.send(error("the engine is not running"));
        }
        rx
    }

    fn subscribe(&self) -> Subscription {
        let rx = match self.ui.upgrade() {
            Some(ui) => ui.subscribe(),
            // The engine stopped: a closed subscription.
            None => broadcast::channel(1).1,
        };
        Subscription { rx }
    }
}

/// A running `ncsyncd` (or another program's engine), over its control
/// endpoint. Each request is one connection; a subscription keeps one open
/// and is read on a thread of its own.
#[derive(Clone, Debug)]
pub struct Remote {
    socket: PathBuf,
}

/// How often a subscription's reader checks that someone still listens.
const SUBSCRIPTION_POLL: Duration = Duration::from_secs(1);

impl Remote {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The engine using `location`'s configuration (its default endpoint).
    pub fn for_location(location: &crate::config_file::ConfigLocation) -> Self {
        Self::new(crate::guard::default_endpoint(location))
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Opens the event stream: `{"command":"subscribe"}`, then `{"ok":true}`.
    fn open_stream(&self) -> std::io::Result<std::io::BufReader<std::os::unix::net::UnixStream>> {
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)?;
        let mut text = serde_json::to_string(&Request::Subscribe).map_err(std::io::Error::other)?;
        text.push('\n');
        stream.write_all(text.as_bytes())?;
        stream.set_read_timeout(Some(SUBSCRIPTION_POLL))?;
        let mut reader = std::io::BufReader::new(stream);
        let mut line = Vec::new();
        let first = read_line(&mut reader, &mut line, || true)?;
        let v: Response =
            serde_json::from_slice(first.unwrap_or_default()).map_err(std::io::Error::other)?;
        if v["ok"] != true {
            return Err(std::io::Error::other(
                v["error"]
                    .as_str()
                    .unwrap_or("subscription refused")
                    .to_owned(),
            ));
        }
        Ok(reader)
    }
}

/// Reads one line, retrying on the read timeout while `go_on()`. `None`:
/// the connection was closed, or `go_on()` said to stop.
fn read_line<'a>(
    reader: &mut impl BufRead,
    line: &'a mut Vec<u8>,
    mut go_on: impl FnMut() -> bool,
) -> std::io::Result<Option<&'a [u8]>> {
    line.clear();
    loop {
        // `read_until` keeps what it read before a timeout in `line`.
        match reader.read_until(b'\n', line) {
            Ok(0) => return Ok(None),
            Ok(_) if line.ends_with(b"\n") => return Ok(Some(line.as_slice())),
            Ok(_) => return Ok(None),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !go_on() {
                    return Ok(None);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

impl EngineClient for Remote {
    fn request(&self, request: Request) -> PendingResponse {
        let (tx, rx) = oneshot::channel();
        let socket = self.socket.clone();
        let spawned = std::thread::Builder::new()
            .name("nc-control".to_owned())
            .spawn(move || {
                let response = crate::control::request(&socket, &request).unwrap_or_else(|e| {
                    error(format!(
                        "cannot reach the engine at {}: {e}",
                        socket.display()
                    ))
                });
                let _ = tx.send(response);
            });
        if let Err(e) = spawned {
            // The sender went with the closure: report through a new pair.
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(error(format!("cannot start a request thread: {e}")));
            return rx;
        }
        rx
    }

    /// Connects and waits for the engine's `{"ok":true}` before it returns,
    /// so every event after the call is seen; the stream is then read on a
    /// thread. A subscription that cannot be opened is closed at once.
    fn subscribe(&self) -> Subscription {
        self.try_subscribe().unwrap_or_else(|e| {
            log::warn!(target: "nextcloud.daemon.client", "cannot subscribe to {}: {e}", self.socket.display());
            Subscription {
                rx: broadcast::channel(1).1,
            }
        })
    }
}

impl Remote {
    /// [`EngineClient::subscribe`], with the reason when the engine cannot
    /// be reached.
    pub fn try_subscribe(&self) -> std::io::Result<Subscription> {
        let (tx, rx) = broadcast::channel(UI_EVENT_CAPACITY);
        let mut reader = self.open_stream()?;
        let _ = std::thread::Builder::new()
            .name("nc-events".to_owned())
            .spawn(move || {
                let mut line = Vec::new();
                loop {
                    let listened = || tx.receiver_count() > 0;
                    let Ok(Some(text)) = read_line(&mut reader, &mut line, listened) else {
                        return;
                    };
                    match serde_json::from_slice::<UiEvent>(text) {
                        Ok(e) => {
                            if tx.send(e).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            log::debug!(target: "nextcloud.daemon.client", "unknown event from the engine: {e}")
                        }
                    }
                }
            });
        // Dropping `tx` (the thread could not start, or it ended) closes
        // the subscription.
        Ok(Subscription { rx })
    }
}
