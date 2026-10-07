/*
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

// Port of upstream test/pushnotificationstestutils.{h,cpp} (nextcloud/desktop v34.0.5).
//
// `FakeWebSocketServer` is a real websocket server on 127.0.0.1 (an
// ephemeral port instead of upstream's fixed 12345, so that the tests can
// run in parallel), plus a `wss` variant with a self-signed certificate.
// `CredentialsStub` is not needed: `nc_dav::Credentials` is plain data.
// `QSignalSpy` is `SignalSpy`, on the broadcast channels of the signals.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nc_dav::{
    Account, BoxFuture, Capabilities, Credentials, HttpClientOptions, PushNotifications, Request,
    Response, ServerUrl, Transport, TransportError,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// `QSignalSpy::wait()`'s default timeout.
const SPY_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages the client sends back to back (user name and password) come in
/// one read for `QWebSocket`, so one `QSignalSpy::wait()` sees them all;
/// here they come one by one: after the first one, the others are collected
/// until none arrives for this long.
const MESSAGES_SETTLE_TIME: Duration = Duration::from_millis(100);

/// `QSignalSpy` on a broadcast channel, counting the events `filter`
/// accepts (the one signal the spy watches).
pub struct SignalSpy<T: Clone> {
    rx: broadcast::Receiver<T>,
    filter: fn(&T) -> bool,
    received: Vec<T>,
}

impl<T: Clone> SignalSpy<T> {
    pub fn new(rx: broadcast::Receiver<T>, filter: fn(&T) -> bool) -> Self {
        Self {
            rx,
            filter,
            received: Vec::new(),
        }
    }

    /// `isValid()`.
    pub fn is_valid(&self) -> bool {
        true
    }

    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(v) => {
                    if (self.filter)(&v) {
                        self.received.push(v);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }

    /// `count()`.
    pub fn count(&mut self) -> usize {
        self.drain();
        self.received.len()
    }

    /// `wait()`: true when the signal is emitted within 5 s.
    pub async fn wait(&mut self) -> bool {
        self.drain();
        let deadline = tokio::time::Instant::now() + SPY_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Ok(v)) => {
                    if (self.filter)(&v) {
                        self.received.push(v);
                        return true;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return false,
            }
        }
    }

    /// `at(i)`.
    pub fn at(&mut self, i: usize) -> T {
        self.drain();
        self.received[i].clone()
    }
}

/// A transport that never answers: the push notification tests make no
/// HTTP request.
struct NullTransport;

impl Transport for NullTransport {
    fn send(&self, _: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        Box::pin(std::future::pending())
    }
}

enum SocketCommand {
    Send(String),
    Abort,
}

/// A connection accepted by the server (the server side `QWebSocket *`).
#[derive(Clone)]
pub struct ServerSocket {
    id: usize,
    commands: mpsc::UnboundedSender<SocketCommand>,
}

impl PartialEq for ServerSocket {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::fmt::Debug for ServerSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ServerSocket({})", self.id)
    }
}

impl ServerSocket {
    /// `sendTextMessage()`.
    pub fn send_text_message(&self, message: &str) {
        let _ = self.commands.send(SocketCommand::Send(message.to_owned()));
    }

    /// `abort()`: drops the TCP connection without a closing handshake.
    pub fn abort(&self) {
        let _ = self.commands.send(SocketCommand::Abort);
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

type Acceptor =
    Arc<dyn Fn(tokio::net::TcpStream) -> BoxFuture<'static, Option<Box<dyn Io>>> + Send + Sync>;

pub struct FakeWebSocketServer {
    url: String,
    secure: bool,
    received: mpsc::UnboundedReceiver<(ServerSocket, String)>,
    text_messages: Vec<(ServerSocket, String)>,
    accept_task: JoinHandle<()>,
    clients: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
}

impl Drop for FakeWebSocketServer {
    fn drop(&mut self) {
        self.close();
    }
}

impl FakeWebSocketServer {
    /// `FakeWebSocketServer()`: a plain `ws://` server.
    pub async fn new() -> Self {
        let acceptor: Acceptor =
            Arc::new(|tcp| Box::pin(async move { Some(Box::new(tcp) as Box<dyn Io>) }));
        Self::listen(acceptor, false).await
    }

    /// A `wss://localhost` server with a self-signed certificate
    /// (`QWebSocketServer::SecureMode`), to test the certificate policy.
    pub async fn new_secure() -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert = certified.cert.der().clone();
        let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
            certified.signing_key.serialize_der().into(),
        );
        let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
        let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let acceptor: Acceptor = Arc::new(move |tcp| {
            let tls = tls.clone();
            Box::pin(async move {
                tls.accept(tcp)
                    .await
                    .ok()
                    .map(|s| Box::new(s) as Box<dyn Io>)
            })
        });
        Self::listen(acceptor, true).await
    }

    async fn listen(acceptor: Acceptor, secure: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = if secure {
            format!("wss://localhost:{port}")
        } else {
            format!("ws://127.0.0.1:{port}")
        };
        let (tx, received) = mpsc::unbounded_channel();
        let clients: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let clients2 = clients.clone();
        let accept_task = tokio::spawn(async move {
            let mut next_id = 0;
            while let Ok((tcp, _)) = listener.accept().await {
                next_id += 1;
                let id = next_id;
                let tx = tx.clone();
                let acceptor = acceptor.clone();
                let task = tokio::spawn(async move {
                    let Some(stream) = acceptor(tcp).await else {
                        return;
                    };
                    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    // onNewConnection()
                    serve_connection(id, ws, tx).await;
                });
                clients2.lock().unwrap().push(task);
            }
        });
        Self {
            url,
            secure,
            received,
            text_messages: Vec::new(),
            accept_task,
            clients,
        }
    }

    /// The websocket URL of the server.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// `authenticateAccount()`. `before_authentication` gets the push
    /// notifications object and returns what `after_authentication` gets
    /// (the spies upstream keeps in captured variables).
    pub async fn authenticate_account_with<T>(
        &mut self,
        account: &Arc<Account>,
        before_authentication: impl FnOnce(&PushNotifications) -> T,
        after_authentication: impl FnOnce(T),
    ) -> Option<ServerSocket> {
        let push_notifications = account
            .push_notifications()
            .expect("the account has push notifications");
        let mut ready_spy = SignalSpy::new(push_notifications.subscribe(), |e| {
            matches!(e, nc_dav::PushNotificationsEvent::Ready)
        });

        let captured = before_authentication(&push_notifications);

        // Wait for authentication
        if !self.wait_for_text_messages().await {
            return None;
        }

        // Right authentication data should be sent
        if self.text_messages_count() != 2 {
            return None;
        }

        let socket = self.socket_for_text_message(0);
        let user_sent = self.text_message(0);
        let password_sent = self.text_message(1);

        let credentials = account.credentials();
        if user_sent != credentials.user || password_sent != credentials.password {
            return None;
        }

        // Sent authenticated
        socket.send_text_message("authenticated");

        // Wait for ready signal
        ready_spy.wait().await;
        if ready_spy.count() != 1 || !account.push_notifications()?.is_ready() {
            return None;
        }

        // Wait for notify_file_id opt-in
        if !self.wait_for_text_messages().await {
            return None;
        }

        after_authentication(captured);

        Some(socket)
    }

    /// `authenticateAccount(account)` with the default (empty) callbacks.
    pub async fn authenticate_account(&mut self, account: &Arc<Account>) -> Option<ServerSocket> {
        self.authenticate_account_with(account, |_| (), |()| ())
            .await
    }

    /// `close()`.
    pub fn close(&mut self) {
        self.accept_task.abort();
        for client in self.clients.lock().unwrap().drain(..) {
            client.abort();
        }
    }

    fn drain(&mut self) {
        while let Ok(m) = self.received.try_recv() {
            self.text_messages.push(m);
        }
    }

    /// `waitForTextMessages()`: true when a text message arrives within 5 s.
    pub async fn wait_for_text_messages(&mut self) -> bool {
        self.drain();
        match tokio::time::timeout(SPY_TIMEOUT, self.received.recv()).await {
            Ok(Some(m)) => self.text_messages.push(m),
            _ => return false,
        }
        while let Ok(Some(m)) =
            tokio::time::timeout(MESSAGES_SETTLE_TIME, self.received.recv()).await
        {
            self.text_messages.push(m);
        }
        true
    }

    /// `textMessagesCount()`.
    pub fn text_messages_count(&mut self) -> u32 {
        self.drain();
        self.text_messages.len() as u32
    }

    /// `textMessage()`.
    pub fn text_message(&mut self, message_number: usize) -> String {
        self.drain();
        assert!(message_number < self.text_messages.len());
        self.text_messages[message_number].1.clone()
    }

    /// `socketForTextMessage()`.
    pub fn socket_for_text_message(&mut self, message_number: usize) -> ServerSocket {
        self.drain();
        assert!(message_number < self.text_messages.len());
        self.text_messages[message_number].0.clone()
    }

    /// `clearTextMessages()`.
    pub fn clear_text_messages(&mut self) {
        self.drain();
        self.text_messages.clear();
    }

    /// `createAccount()` with the default arguments, the websocket URL being
    /// this server's (an `https://localhost` account for the secure server).
    pub fn create_account(&self) -> Arc<Account> {
        let account_url = if self.secure {
            "https://localhost"
        } else {
            "http://localhost"
        };
        Self::create_account_with(
            "user",
            "password",
            account_url,
            &self.url,
            HttpClientOptions::default(),
        )
    }

    /// `createAccount(username, password, accountUrl, webSocketUrl)`, plus
    /// the options of the account's HTTP client (`--trust`, proxy), which
    /// the websocket uses too.
    pub fn create_account_with(
        username: &str,
        password: &str,
        account_url: &str,
        web_socket_url: &str,
        options: HttpClientOptions,
    ) -> Arc<Account> {
        let account = Arc::new(Account::new(
            ServerUrl::parse(account_url).unwrap(),
            Arc::new(NullTransport),
        ));
        // `Account::create()`: the account owns its push notifications.
        account.enable_push_notifications(options);

        let capabilities_map = serde_json::json!({
            "notify_push": {
                "type": ["files", "activities", "notifications"],
                "endpoints": { "websocket": web_socket_url },
            }
        });
        account.set_capabilities(Capabilities::from_json(capabilities_map));

        account.set_credentials(Credentials::new(username, password));

        account
    }
}

async fn serve_connection(
    id: usize,
    mut ws: tokio_tungstenite::WebSocketStream<Box<dyn Io>>,
    tx: mpsc::UnboundedSender<(ServerSocket, String)>,
) {
    let (commands, mut commands_rx) = mpsc::unbounded_channel();
    let socket = ServerSocket { id, commands };
    loop {
        tokio::select! {
            command = commands_rx.recv() => match command {
                Some(SocketCommand::Send(text)) => {
                    if ws.send(Message::text(text)).await.is_err() {
                        return;
                    }
                }
                // Dropping the stream closes the TCP connection without a
                // websocket closing handshake.
                Some(SocketCommand::Abort) | None => return,
            },
            message = ws.next() => match message {
                // processTextMessageInternal()
                Some(Ok(Message::Text(text))) => {
                    let _ = tx.send((socket.clone(), text.as_str().to_owned()));
                }
                Some(Ok(_)) => {}
                // socketDisconnected()
                Some(Err(_)) | None => return,
            },
        }
    }
}
