// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/pushnotifications.cpp` (nextcloud/desktop v34.0.5).

//! The `notify_push` client (`PushNotifications`): a websocket to the
//! endpoint the server advertises in its capabilities, authenticated with the
//! user name and the (app) password, which tells when files, activities or
//! notifications changed on the server.
//!
//! # Mapping of the upstream `QObject`
//!
//! Upstream's object owns a `QWebSocket` and three `QTimer`s and lives on the
//! Qt event loop. Here a [`PushNotifications`] is a cheap handle (it can be
//! cloned) on one task (`tokio::spawn`) that owns the websocket and the
//! timers. The task handles one wake-up at a time (a command, a websocket
//! event or a timer), in that priority order, so it is as sequential as the
//! Qt slots.
//!
//! * The setters (`setReconnectTimerInterval`, `setPingInterval`) and
//!   `setup()` take effect in the order they are called, before any websocket
//!   event that arrives later.
//! * `isReady()` is read without waiting ([`PushNotifications::is_ready`]).
//! * The signals are [`PushNotificationsEvent`]s on a `tokio::sync::broadcast`
//!   channel: [`PushNotifications::subscribe`] is the `connect()` (or the
//!   `QSignalSpy`) and, like a Qt connection, a receiver only sees what is
//!   emitted after it subscribed. Several receivers can listen, as several
//!   slots can be connected. The [`crate::Account`] that owns the object
//!   reacts to them first, synchronously, like its connections made at
//!   construction (see [`crate::Account::try_setup_push_notifications`]).
//! * Deleting the object (`delete _pushNotifications`) is
//!   [`PushNotifications::close`]; the task also ends when the last handle is
//!   dropped (the destructor closes the websocket).
//!
//! The account is only referenced weakly (upstream keeps a raw `Account *`
//! owned by the account itself).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_rustls::rustls;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::account::{Account, ServerUrl};
use crate::http_client::HttpClientOptions;

/// Logging category `nextcloud.sync.pushnotifications`.
const LOG: &str = "nextcloud.sync.pushnotifications";

/// `MAX_ALLOWED_FAILED_AUTHENTICATION_ATTEMPTS`.
const MAX_ALLOWED_FAILED_AUTHENTICATION_ATTEMPTS: u8 = 3;
/// `PING_INTERVAL`: the default interval of `_pingTimer` and
/// `_pingTimedOutTimer`.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// The default `_reconnectTimerInterval`.
pub const RECONNECT_TIMER_INTERVAL: Duration = Duration::from_secs(20);
/// `NOTIFY_FILE_ID_PREFIX`.
const NOTIFY_FILE_ID_PREFIX: &str = "notify_file_id ";

/// Room for events not yet read by a slow receiver (it then gets
/// `RecvError::Lagged`); Qt connections never drop signals.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// How long a closed websocket may take to finish its closing handshake in
/// the background (`QWebSocket::close()` does it asynchronously too).
const CLOSE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The signals of upstream `PushNotifications`.
#[derive(Clone, Debug)]
pub enum PushNotificationsEvent {
    /// `ready()`: connected and authenticated.
    Ready,
    /// `filesChanged(Account *)`.
    FilesChanged { account: Weak<Account> },
    /// `fileIdsChanged(Account *, const QList<qint64> &)`.
    FileIdsChanged {
        account: Weak<Account>,
        file_ids: Vec<i64>,
    },
    /// `activitiesChanged(Account *)`.
    ActivitiesChanged { account: Weak<Account> },
    /// `notificationsChanged(Account *)`.
    NotificationsChanged { account: Weak<Account> },
    /// `authenticationFailed()`. It is safe to call
    /// [`PushNotifications::setup`] afterwards.
    AuthenticationFailed,
    /// `connectionLost()`. It is safe to call [`PushNotifications::setup`]
    /// afterwards.
    ConnectionLost,
}

impl PushNotificationsEvent {
    /// The `Account *` argument of the signals that carry one.
    pub fn account(&self) -> Option<&Weak<Account>> {
        match self {
            Self::FilesChanged { account }
            | Self::FileIdsChanged { account, .. }
            | Self::ActivitiesChanged { account }
            | Self::NotificationsChanged { account } => Some(account),
            Self::Ready | Self::AuthenticationFailed | Self::ConnectionLost => None,
        }
    }
}

/// A slot called synchronously for every signal, before the broadcast
/// receivers see it (the account's own connections).
pub(crate) type SignalHook = Box<dyn Fn(&PushNotificationsEvent) + Send + Sync>;

enum Command {
    Setup,
    SetPingInterval(Duration),
    Close,
}

struct Shared {
    is_ready: AtomicBool,
    /// `_reconnectTimerInterval`, in milliseconds.
    reconnect_timer_interval_ms: AtomicU64,
    events: broadcast::Sender<PushNotificationsEvent>,
}

/// The `notify_push` client (`PushNotifications`). See the module
/// documentation for how the Qt object is mapped.
#[derive(Clone)]
pub struct PushNotifications {
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
}

impl std::fmt::Debug for PushNotifications {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushNotifications")
            .field("is_ready", &self.is_ready())
            .finish_non_exhaustive()
    }
}

impl PushNotifications {
    /// `PushNotifications(Account *)`. The websocket is opened by
    /// [`Self::setup`]. `options` are those of the account's
    /// [`crate::HttpTransport`]: the certificate policy (`--trust`) and the
    /// HTTP proxy apply to the websocket too.
    ///
    /// Spawns the task with `tokio::spawn`: must be called inside a Tokio
    /// runtime.
    pub fn new(account: &Arc<Account>, options: HttpClientOptions) -> Self {
        Self::with_hook(Arc::downgrade(account), options, None)
    }

    pub(crate) fn with_hook(
        account: Weak<Account>,
        options: HttpClientOptions,
        hook: Option<SignalHook>,
    ) -> Self {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let shared = Arc::new(Shared {
            is_ready: AtomicBool::new(false),
            reconnect_timer_interval_ms: AtomicU64::new(RECONNECT_TIMER_INTERVAL.as_millis() as u64),
            events,
        });
        let actor = Actor {
            account,
            shared: shared.clone(),
            options,
            hook,
            tls_config: None,
            failed_authentication_attempts_count: 0,
            reconnect_timer: None,
            ping_timer_interval: PING_INTERVAL,
            ping_timer: None,
            ping_timed_out_timer_interval: PING_INTERVAL,
            ping_timed_out_timer: None,
            pong_received_from_web_socket_server: false,
            pending_socket_error: None,
        };
        tokio::spawn(actor.run(commands_rx));
        Self { shared, commands }
    }

    /// `setup()`: (re)connects and authenticates. Resets the count of failed
    /// authentication attempts.
    pub fn setup(&self) {
        let _ = self.commands.send(Command::Setup);
    }

    /// `setReconnectTimerInterval()`: the delay before reconnecting after
    /// `err: Invalid credentials`. Read when the reconnection is scheduled.
    pub fn set_reconnect_timer_interval(&self, interval: Duration) {
        self.shared
            .reconnect_timer_interval_ms
            .store(interval.as_millis() as u64, Ordering::SeqCst);
    }

    /// `isReady()`: connected and authenticated.
    pub fn is_ready(&self) -> bool {
        self.shared.is_ready.load(Ordering::SeqCst)
    }

    /// `setPingInterval()`: the interval of both the ping timer and the
    /// pong timeout. Like `QTimer::setInterval`, a running timer restarts
    /// with the new interval.
    pub fn set_ping_interval(&self, interval: Duration) {
        let _ = self.commands.send(Command::SetPingInterval(interval));
    }

    /// Connects to the signals: the returned receiver gets every
    /// [`PushNotificationsEvent`] emitted from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<PushNotificationsEvent> {
        self.shared.events.subscribe()
    }

    /// `delete pushNotifications` (`~PushNotifications`): closes the
    /// websocket and ends the task. No signal is emitted afterwards.
    pub fn close(&self) {
        let _ = self.commands.send(Command::Close);
    }

    /// Whether both handles refer to the same object.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Boxed: the stream is large and moves between states.
type WebSocket = Box<WebSocketStream<Box<dyn Io>>>;
type OpenFuture = Pin<Box<dyn Future<Output = Result<WebSocket, OpenError>> + Send>>;

/// Why opening the websocket failed, split like the two `QWebSocket`
/// signals upstream handles differently.
#[derive(Debug)]
enum OpenError {
    /// `sslErrors`: the server certificate was not accepted.
    Ssl(String),
    /// `errorOccurred`: everything else (refused, unreachable, TLS handshake
    /// failure, HTTP upgrade refused, ...).
    Socket(String),
}

/// The state of the `QWebSocket`.
enum Socket {
    Unconnected,
    Connecting(OpenFuture),
    Connected(WebSocket),
}

enum SocketEvent {
    Opened(Result<WebSocket, OpenError>),
    Message(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
}

enum Wake {
    Command(Option<Command>),
    Socket(SocketEvent),
    ReconnectTimer,
    PingTimer,
    PingTimedOutTimer,
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

struct Actor {
    account: Weak<Account>,
    shared: Arc<Shared>,
    options: HttpClientOptions,
    hook: Option<SignalHook>,
    tls_config: Option<Arc<rustls::ClientConfig>>,
    failed_authentication_attempts_count: u8,
    /// `_reconnectTimer` (single shot): its deadline when active.
    reconnect_timer: Option<Instant>,
    ping_timer_interval: Duration,
    /// `_pingTimer` (single shot).
    ping_timer: Option<Instant>,
    ping_timed_out_timer_interval: Duration,
    /// `_pingTimedOutTimer` (single shot).
    ping_timed_out_timer: Option<Instant>,
    pong_received_from_web_socket_server: bool,
    /// A failed send: like `QWebSocket`, the error is reported after the
    /// current slot returned.
    pending_socket_error: Option<String>,
}

impl Actor {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut socket = Socket::Unconnected;
        loop {
            let (reconnect, ping, ping_timed_out) = (
                self.reconnect_timer,
                self.ping_timer,
                self.ping_timed_out_timer,
            );
            let socket_event = async {
                match &mut socket {
                    Socket::Connecting(open) => SocketEvent::Opened(open.as_mut().await),
                    Socket::Connected(ws) => SocketEvent::Message(ws.next().await),
                    Socket::Unconnected => std::future::pending().await,
                }
            };
            let wake = tokio::select! {
                biased;
                c = commands.recv() => Wake::Command(c),
                e = socket_event => Wake::Socket(e),
                () = sleep_until(reconnect) => Wake::ReconnectTimer,
                () = sleep_until(ping) => Wake::PingTimer,
                () = sleep_until(ping_timed_out) => Wake::PingTimedOutTimer,
            };
            match wake {
                Wake::Command(None) | Wake::Command(Some(Command::Close)) => {
                    // ~PushNotifications()
                    self.close_web_socket(&mut socket);
                    return;
                }
                Wake::Command(Some(Command::Setup)) => self.setup(&mut socket).await,
                Wake::Command(Some(Command::SetPingInterval(interval))) => {
                    self.set_ping_interval(interval)
                }
                Wake::Socket(SocketEvent::Opened(Ok(ws))) => {
                    socket = Socket::Connected(ws);
                    self.on_web_socket_connected(&mut socket).await;
                }
                Wake::Socket(SocketEvent::Opened(Err(OpenError::Ssl(e)))) => {
                    socket = Socket::Unconnected;
                    self.on_web_socket_ssl_errors(&mut socket, &e);
                }
                Wake::Socket(SocketEvent::Opened(Err(OpenError::Socket(e)))) => {
                    socket = Socket::Unconnected;
                    self.on_web_socket_error(&mut socket, &e);
                }
                Wake::Socket(SocketEvent::Message(Some(Ok(message)))) => match message {
                    Message::Text(text) => {
                        self.on_web_socket_text_message_received(&mut socket, text.as_str())
                            .await
                    }
                    Message::Pong(_) => self.on_web_socket_pong_received(),
                    // The closing handshake is answered by tungstenite; the
                    // stream then ends (`disconnected`).
                    Message::Close(_)
                    | Message::Ping(_)
                    | Message::Binary(_)
                    | Message::Frame(_) => {}
                },
                Wake::Socket(SocketEvent::Message(Some(Err(e)))) => {
                    socket = Socket::Unconnected;
                    self.on_web_socket_error(&mut socket, &e.to_string());
                }
                Wake::Socket(SocketEvent::Message(None)) => {
                    socket = Socket::Unconnected;
                    self.on_web_socket_disconnected();
                }
                Wake::ReconnectTimer => {
                    self.reconnect_timer = None;
                    self.reconnect_to_web_socket(&mut socket);
                }
                Wake::PingTimer => {
                    self.ping_timer = None;
                    self.ping_web_socket_server(&mut socket).await;
                }
                Wake::PingTimedOutTimer => {
                    self.ping_timed_out_timer = None;
                    self.on_ping_timed_out(&mut socket).await;
                }
            }
            if let Some(e) = self.pending_socket_error.take()
                && matches!(socket, Socket::Connected(_))
            {
                socket = Socket::Unconnected;
                self.on_web_socket_error(&mut socket, &e);
            }
        }
    }

    fn account_url(&self) -> String {
        self.account
            .upgrade()
            .map(|a| a.url().to_credential_free_string())
            .unwrap_or_default()
    }

    fn account_display_name(&self) -> String {
        self.account
            .upgrade()
            .map(|a| a.dav_display_name())
            .unwrap_or_default()
    }

    fn emit(&self, event: PushNotificationsEvent) {
        if let Some(hook) = &self.hook {
            hook(&event);
        }
        // No receiver is not an error (a signal without connection).
        let _ = self.shared.events.send(event);
    }

    async fn setup(&mut self, socket: &mut Socket) {
        log::info!(target: LOG, "Setup push notifications");
        self.failed_authentication_attempts_count = 0;
        self.reconnect_to_web_socket(socket);
    }

    fn reconnect_to_web_socket(&mut self, socket: &mut Socket) {
        self.close_web_socket(socket);
        self.open_web_socket(socket);
    }

    fn close_web_socket(&mut self, socket: &mut Socket) {
        log::info!(target: LOG, "Close websocket for account {}", self.account_url());

        self.ping_timer = None;
        self.ping_timed_out_timer = None;
        self.shared.is_ready.store(false, Ordering::SeqCst);

        // Maybe there run some reconnection attempts
        self.reconnect_timer = None;

        // Upstream disconnects the error signals first: nothing that happens
        // to the old socket is reported any more.
        self.pending_socket_error = None;
        if let Socket::Connected(mut ws) = std::mem::replace(socket, Socket::Unconnected) {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(CLOSE_HANDSHAKE_TIMEOUT, async {
                    let _ = WebSocketStream::close(&mut ws, None).await;
                    while let Some(Ok(_)) = ws.next().await {}
                })
                .await;
            });
        }
    }

    async fn on_web_socket_connected(&mut self, socket: &mut Socket) {
        log::info!(target: LOG, "Connected to websocket for account {}", self.account_url());
        self.authenticate_on_web_socket(socket).await;
    }

    async fn send_text_message(&mut self, socket: &mut Socket, text: &str) {
        if let Socket::Connected(ws) = socket
            && let Err(e) = ws.send(Message::text(text)).await
            && self.pending_socket_error.is_none()
        {
            self.pending_socket_error = Some(e.to_string());
        }
    }

    async fn authenticate_on_web_socket(&mut self, socket: &mut Socket) {
        let credentials = self
            .account
            .upgrade()
            .map(|a| a.credentials())
            .unwrap_or_default();

        // Authenticate
        self.send_text_message(socket, &credentials.user).await;
        self.send_text_message(socket, &credentials.password).await;
    }

    fn on_web_socket_disconnected(&mut self) {
        log::info!(target: LOG, "Disconnected from websocket for account {}", self.account_url());
    }

    async fn on_web_socket_text_message_received(&mut self, socket: &mut Socket, message: &str) {
        log::info!(target: LOG, "Received push notification: {message:?}");

        if message.starts_with(NOTIFY_FILE_ID_PREFIX) {
            self.handle_notify_file_id(message);
        } else if message == "notify_file" {
            self.handle_notify_file();
        } else if message == "notify_activity" {
            self.handle_notify_activity();
        } else if message == "notify_notification" {
            self.handle_notify_notification();
        } else if message == "authenticated" {
            self.handle_authenticated(socket).await;
        } else if message == "err: Invalid credentials" {
            self.handle_invalid_credentials(socket);
        }
    }

    fn on_web_socket_error(&mut self, socket: &mut Socket, error: &str) {
        log::warn!(
            target: LOG,
            "Websocket error on with account {} {} {error}",
            self.account_display_name(),
            self.account_url()
        );
        self.close_web_socket(socket);
        self.emit(PushNotificationsEvent::ConnectionLost);
    }

    fn try_reconnect_to_web_socket(&mut self) -> bool {
        self.failed_authentication_attempts_count += 1;
        if self.failed_authentication_attempts_count >= MAX_ALLOWED_FAILED_AUTHENTICATION_ATTEMPTS {
            log::info!(target: LOG, "Max authentication attempts reached");
            return false;
        }

        let interval = Duration::from_millis(
            self.shared
                .reconnect_timer_interval_ms
                .load(Ordering::SeqCst),
        );
        self.reconnect_timer = Some(Instant::now() + interval);

        true
    }

    fn on_web_socket_ssl_errors(&mut self, socket: &mut Socket, errors: &str) {
        log::warn!(
            target: LOG,
            "Websocket ssl errors on with account {} {} {errors}",
            self.account_display_name(),
            self.account_url()
        );
        self.close_web_socket(socket);
        self.emit(PushNotificationsEvent::AuthenticationFailed);
    }

    fn tls_config(&mut self) -> Result<Arc<rustls::ClientConfig>, String> {
        if let Some(config) = &self.tls_config {
            return Ok(config.clone());
        }
        let config = build_tls_config(self.options.trust_invalid_certificates)?;
        self.tls_config = Some(config.clone());
        Ok(config)
    }

    fn open_web_socket(&mut self, socket: &mut Socket) {
        // Open websocket
        let Some(account) = self.account.upgrade() else {
            return;
        };
        let web_socket_url = account.capabilities().push_notifications_web_socket_url();

        log::info!(
            target: LOG,
            "Open connection to websocket on {web_socket_url} for account {} {}",
            account.dav_display_name(),
            account.url().to_credential_free_string()
        );
        let tls = if web_socket_url
            .get(..4)
            .is_some_and(|s| s.eq_ignore_ascii_case("wss:"))
        {
            Some(self.tls_config())
        } else {
            None
        };
        *socket = Socket::Connecting(Box::pin(open(
            web_socket_url,
            self.options.proxy.clone(),
            tls,
        )));
    }

    async fn handle_authenticated(&mut self, socket: &mut Socket) {
        log::info!(target: LOG, "Authenticated successful on websocket");
        self.failed_authentication_attempts_count = 0;
        log::debug!(target: LOG, "Requesting opt-in to 'notify_file_id' notifications");
        self.send_text_message(socket, "listen notify_file_id")
            .await;
        self.shared.is_ready.store(true, Ordering::SeqCst);
        self.start_ping_timer();
        self.emit(PushNotificationsEvent::Ready);

        // We maybe reconnected to websocket while being offline for a
        // while. To not miss any notifications that may have happened,
        // emit all the signals once.
        self.emit_files_changed();
        self.emit_notifications_changed();
        self.emit_activities_changed();
    }

    fn handle_notify_file(&self) {
        log::info!(target: LOG, "Files push notification arrived");
        self.emit_files_changed();
    }

    fn handle_notify_file_id(&self, message: &str) {
        log::debug!(target: LOG, "File-ID push notification arrived");

        let file_ids_json = &message[NOTIFY_FILE_ID_PREFIX.len()..];
        let json_doc: serde_json::Value = match serde_json::from_str(file_ids_json) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    target: LOG,
                    "could not parse received list of file IDs error={e} fileIdsJson={file_ids_json}"
                );
                return;
            }
        };

        let file_ids: Vec<i64> = match &json_doc {
            serde_json::Value::Array(json_array) => json_array
                .iter()
                .filter_map(|json_fileid| match json_fileid {
                    serde_json::Value::Number(n) => Some(q_json_value_to_integer(n)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };

        if file_ids.is_empty() {
            log::debug!(
                target: LOG,
                "Cancelling file IDs changed signal emission due to empty list of file IDs."
            );
            return;
        }

        log::debug!(target: LOG, "Emitting signal of changed file IDs.");
        self.emit(PushNotificationsEvent::FileIdsChanged {
            account: self.account.clone(),
            file_ids,
        });
    }

    fn handle_invalid_credentials(&mut self, socket: &mut Socket) {
        log::info!(target: LOG, "Invalid credentials submitted to websocket");
        if !self.try_reconnect_to_web_socket() {
            self.close_web_socket(socket);
            self.emit(PushNotificationsEvent::AuthenticationFailed);
        }
    }

    fn handle_notify_notification(&self) {
        log::info!(target: LOG, "Push notification arrived");
        self.emit_notifications_changed();
    }

    fn handle_notify_activity(&self) {
        log::info!(target: LOG, "Push activity arrived");
        self.emit_activities_changed();
    }

    fn on_web_socket_pong_received(&mut self) {
        log::debug!(target: LOG, "Pong received in time");
        // We are fine with every kind of pong and don't care about the
        // payload. As long as we receive pongs the server is still alive.
        self.pong_received_from_web_socket_server = true;
        self.start_ping_timer();
    }

    fn start_ping_timer(&mut self) {
        self.ping_timed_out_timer = None;
        self.ping_timer = Some(Instant::now() + self.ping_timer_interval);
    }

    fn start_ping_timed_out_timer(&mut self) {
        self.ping_timed_out_timer = Some(Instant::now() + self.ping_timed_out_timer_interval);
    }

    async fn ping_web_socket_server(&mut self, socket: &mut Socket) {
        log::debug!(target: LOG, "Ping websocket server");

        self.pong_received_from_web_socket_server = false;

        // `QWebSocket::ping()` does nothing on a socket that is not connected.
        if let Socket::Connected(ws) = socket
            && let Err(e) = ws.send(Message::Ping(Default::default())).await
            && self.pending_socket_error.is_none()
        {
            self.pending_socket_error = Some(e.to_string());
        }
        self.start_ping_timed_out_timer();
    }

    async fn on_ping_timed_out(&mut self, socket: &mut Socket) {
        if self.pong_received_from_web_socket_server {
            log::debug!(target: LOG, "Websocket respond with a pong in time.");
            return;
        }

        log::info!(target: LOG, "Websocket did not respond with a pong in time. Try to reconnect.");
        // Try again to connect
        self.setup(socket).await;
    }

    fn set_ping_interval(&mut self, timeout_interval: Duration) {
        // `QTimer::setInterval` restarts an active timer.
        self.ping_timer_interval = timeout_interval;
        if self.ping_timer.is_some() {
            self.ping_timer = Some(Instant::now() + timeout_interval);
        }
        self.ping_timed_out_timer_interval = timeout_interval;
        if self.ping_timed_out_timer.is_some() {
            self.ping_timed_out_timer = Some(Instant::now() + timeout_interval);
        }
    }

    fn emit_files_changed(&self) {
        self.emit(PushNotificationsEvent::FilesChanged {
            account: self.account.clone(),
        });
    }

    fn emit_notifications_changed(&self) {
        self.emit(PushNotificationsEvent::NotificationsChanged {
            account: self.account.clone(),
        });
    }

    fn emit_activities_changed(&self) {
        self.emit(PushNotificationsEvent::ActivitiesChanged {
            account: self.account.clone(),
        });
    }
}

/// `QJsonValue::toInteger()` of a number (`isDouble()`): the value when it
/// is an integer representable as `qint64`, else 0.
fn q_json_value_to_integer(n: &serde_json::Number) -> i64 {
    if let Some(i) = n.as_i64() {
        return i;
    }
    match n.as_f64() {
        Some(d) if d.fract() == 0.0 && d >= i64::MIN as f64 && d < i64::MAX as f64 => d as i64,
        _ => 0,
    }
}

/// The TLS configuration of the websocket, built like reqwest's rustls
/// client in [`crate::HttpTransport`]: the default crypto provider (or
/// aws-lc-rs), TLS 1.2 and 1.3, certificates checked by the platform
/// verifier, or not at all with `--trust`.
fn build_tls_config(trust_invalid_certificates: bool) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = if trust_invalid_certificates {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier(provider)))
            .with_no_client_auth()
    } else {
        let verifier =
            rustls_platform_verifier::Verifier::new(provider).map_err(|e| e.to_string())?;
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// Accepts every certificate (`--trust`), like reqwest's
/// `danger_accept_invalid_certs`.
#[derive(Debug)]
struct NoVerifier(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `QWebSocket::open(url)`: TCP (through the HTTP proxy if any), TLS for
/// `wss`, then the HTTP upgrade.
async fn open(
    url: String,
    proxy: Option<String>,
    tls: Option<Result<Arc<rustls::ClientConfig>, String>>,
) -> Result<WebSocket, OpenError> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e| OpenError::Socket(format!("invalid websocket URL {url:?}: {e}")))?;
    let secure = match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("wss") => true,
        Some("ws") => false,
        _ => {
            return Err(OpenError::Socket(format!(
                "unsupported websocket URL scheme in {url:?}"
            )));
        }
    };
    let host = uri
        .host()
        .ok_or_else(|| OpenError::Socket(format!("no host in websocket URL {url:?}")))?;
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });

    let tcp = match proxy {
        Some(proxy) => connect_through_proxy(&proxy, host, port).await?,
        None => TcpStream::connect((bare_host, port))
            .await
            .map_err(|e| OpenError::Socket(e.to_string()))?,
    };
    let _ = tcp.set_nodelay(true);

    let stream: Box<dyn Io> = if secure {
        let config = tls
            .unwrap_or_else(|| Err("no TLS configuration".to_owned()))
            .map_err(OpenError::Socket)?;
        let server_name = rustls::pki_types::ServerName::try_from(bare_host.to_owned())
            .map_err(|e| OpenError::Socket(e.to_string()))?;
        let tls_stream = tokio_rustls::TlsConnector::from(config)
            .connect(server_name, tcp)
            .await
            .map_err(classify_tls_error)?;
        Box::new(tls_stream)
    } else {
        Box::new(tcp)
    };

    let (ws, _response) = tokio_tungstenite::client_async(url.as_str(), stream)
        .await
        .map_err(|e| OpenError::Socket(e.to_string()))?;
    Ok(Box::new(ws))
}

/// A rejected certificate is `QWebSocket::sslErrors`; any other TLS failure
/// is `errorOccurred(SslHandshakeFailedError)`.
fn classify_tls_error(e: std::io::Error) -> OpenError {
    if let Some(inner) = e.get_ref()
        && let Some(rustls::Error::InvalidCertificate(_)) = inner.downcast_ref::<rustls::Error>()
    {
        return OpenError::Ssl(e.to_string());
    }
    OpenError::Socket(e.to_string())
}

/// An HTTP `CONNECT` tunnel through `--httpproxy` (`http://[user:pass@]host[:port]`),
/// what `QWebSocket` does through the application proxy.
async fn connect_through_proxy(proxy: &str, host: &str, port: u16) -> Result<TcpStream, OpenError> {
    let with_scheme = if proxy.contains("://") {
        proxy.to_owned()
    } else {
        format!("http://{proxy}")
    };
    let proxy_url = ServerUrl::parse(&with_scheme).map_err(|e| OpenError::Socket(e.to_string()))?;
    if proxy_url.scheme != "http" {
        return Err(OpenError::Socket(format!(
            "unsupported proxy scheme {:?} for the websocket",
            proxy_url.scheme
        )));
    }
    let proxy_port = proxy_url
        .authority
        .strip_prefix(&proxy_url.host)
        .and_then(|rest| rest.strip_prefix(':'))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    let proxy_host = proxy_url.host.trim_start_matches('[').trim_end_matches(']');
    let mut tcp = TcpStream::connect((proxy_host, proxy_port))
        .await
        .map_err(|e| OpenError::Socket(e.to_string()))?;

    let mut request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if !proxy_url.user.is_empty() {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", proxy_url.user, proxy_url.password));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    tcp.write_all(request.as_bytes())
        .await
        .map_err(|e| OpenError::Socket(e.to_string()))?;

    // Read the response head byte by byte: nothing after it belongs to us.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err(OpenError::Socket("proxy response too long".to_owned()));
        }
        let b = tcp
            .read_u8()
            .await
            .map_err(|e| OpenError::Socket(e.to_string()))?;
        head.push(b);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        let status_line = head.lines().next().unwrap_or_default();
        return Err(OpenError::Socket(format!(
            "proxy refused the tunnel: {status_line}"
        )));
    }
    Ok(tcp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_file_id_to_integer() {
        let n = |s: &str| serde_json::from_str::<serde_json::Number>(s).unwrap();
        assert_eq!(q_json_value_to_integer(&n("42")), 42);
        assert_eq!(q_json_value_to_integer(&n("-7")), -7);
        assert_eq!(q_json_value_to_integer(&n("3.0")), 3);
        assert_eq!(q_json_value_to_integer(&n("1.5")), 0);
        assert_eq!(q_json_value_to_integer(&n("18446744073709551615")), 0);
    }
}
