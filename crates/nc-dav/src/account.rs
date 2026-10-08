// SPDX-FileCopyrightText: 2016 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the parts of upstream `src/libsync/account.cpp` and
// `src/libsync/creds/httpcredentials.cpp` (nextcloud/desktop v34.0.5) the sync
// engine and `nextcloudcmd` need, and of the push notifications ownership of
// `account.cpp` (`trySetupPushNotifications` and its reconnect timer).

//! The account: server URL, credentials, user, server version and
//! capabilities, plus the transport every request goes through.

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use base64::Engine as _;
use http::{HeaderMap, HeaderValue, Method};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use crate::capabilities::{Capabilities, PushNotificationTypes};
use crate::client_status::{ClientStatusReporter, ClientStatusReportingStatus};
use crate::http_client::HttpClientOptions;
use crate::push_notifications::{PushNotifications, PushNotificationsEvent};
use crate::transport::{
    Body, IgnoreCredentialFailure, Request, Response, Transport, TransportError,
};

/// What `Account::handleInvalidCredentials()` notifies (see
/// [`Account::set_invalid_credentials_handler`]).
pub type InvalidCredentialsHandler = Arc<dyn Fn() + Send + Sync>;

/// Creates the `ClientStatusReporting` of an account (its database and its
/// periodic sender); `None` when it cannot be set up.
pub type ClientStatusReportingFactory =
    Arc<dyn Fn(&Arc<Account>) -> Option<Arc<dyn ClientStatusReporter>> + Send + Sync>;

#[derive(Default)]
struct StatusReportingState {
    /// Set by [`Account::enable_client_status_reporting`].
    factory: Option<(ClientStatusReportingFactory, Weak<Account>)>,
    reporter: Option<Arc<dyn ClientStatusReporter>>,
}

/// Characters `QUrl` percent-encodes in a path when sending it.
pub const PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Characters `QUrl::toPercentEncoding(s, "/")` leaves alone: unreserved
/// (`ALPHA DIGIT - . _ ~`) plus `/`.
pub const TO_PERCENT_ENCODING_SLASH: &AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// `QUrl::toPercentEncoding(s, "/")`.
pub fn to_percent_encoding_keep_slash(s: &str) -> String {
    utf8_percent_encode(s, TO_PERCENT_ENCODING_SLASH).to_string()
}

/// `Utility::concatUrlPath` on paths: joins avoiding `//` and a missing `/`.
pub fn concat_url_path(path: &str, concat_path: &str) -> String {
    let mut path = path.to_owned();
    if !concat_path.is_empty() {
        if path.ends_with('/') && concat_path.starts_with('/') {
            path.pop();
        } else if !path.ends_with('/') && !concat_path.starts_with('/') {
            path.push('/');
        }
        path.push_str(concat_path);
    }
    path
}

/// Characters `QUrlQuery` percent-encodes in a query item key or value
/// (`QUrl::toEncoded()`): those of a path, minus `?` (allowed in a query),
/// plus the item delimiters `&` and `=`.
pub const QUERY_ITEM_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'&')
    .add(b'=');

/// `QUrlQuery::setQueryItems(items)` then `query(QUrl::FullyEncoded)`:
/// `key=value` pairs joined with `&`.
pub fn encode_query_items(items: &[(String, String)]) -> String {
    items
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, QUERY_ITEM_ENCODE_SET),
                utf8_percent_encode(v, QUERY_ITEM_ENCODE_SET)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Characters `QUrl::fromUserInput` (tolerant mode) percent-encodes in a
/// URL given as text: those never valid in a URI. `%`, `#`, `?` and the
/// delimiters are kept as given.
const USER_INPUT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// `QUrl::fromUserInput(text)` for the URLs the client gets from the
/// server (direct download URLs): a text without a scheme is taken as an
/// `http://` URL, invalid characters are percent-encoded, a fragment is
/// dropped (it is never sent).
pub fn url_from_user_input(text: &str) -> Result<http::Uri, UrlError> {
    let trimmed = text.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    };
    let without_fragment = with_scheme.split('#').next().unwrap_or_default();
    let encoded = utf8_percent_encode(without_fragment, USER_INPUT_ENCODE_SET).to_string();
    http::Uri::try_from(encoded.as_str()).map_err(|_| UrlError(text.to_owned()))
}

/// A server URL split like `QUrl`: scheme, authority (without user info),
/// decoded path, and the user info.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerUrl {
    pub scheme: String,
    /// `host[:port]`.
    pub authority: String,
    pub host: String,
    /// Decoded path, e.g. `/owncloud` (may be empty).
    pub path: String,
    pub user: String,
    pub password: String,
}

/// Errors parsing a server URL.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid server URL: {0}")]
pub struct UrlError(pub String);

impl ServerUrl {
    /// Parses `scheme://[user[:password]@]host[:port][/path]`. A URL without
    /// scheme gets `https://` (`QUrl::fromUserInput`).
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        let input = input.trim();
        let (scheme, rest) = match input.split_once("://") {
            Some((s, r)) => (s.to_ascii_lowercase(), r),
            None => ("https".to_owned(), input),
        };
        if scheme.is_empty() {
            return Err(UrlError(input.to_owned()));
        }
        let (authority_full, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (userinfo, authority) = match authority_full.rsplit_once('@') {
            Some((u, a)) => (Some(u), a),
            None => (None, authority_full),
        };
        if authority.is_empty() {
            return Err(UrlError(input.to_owned()));
        }
        // QUrl normalizes the host to lower case.
        let authority = authority.to_ascii_lowercase();
        let authority = authority.as_str();
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((u, p)) => (decode(u), decode(p)),
                None => (decode(u), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let host = if authority.starts_with('[') {
            authority
                .split(']')
                .next()
                .map(|h| format!("{h}]"))
                .unwrap_or_default()
        } else {
            authority.split(':').next().unwrap_or("").to_owned()
        };
        // Strip query and fragment.
        let path = path.split(['?', '#']).next().unwrap_or("");
        Ok(Self {
            scheme,
            authority: authority.to_owned(),
            host,
            path: decode(path),
            user,
            password,
        })
    }

    /// The URL without user info (`credentialFreeUrl`), as text.
    pub fn to_credential_free_string(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme,
            self.authority,
            utf8_percent_encode(&self.path, PATH_ENCODE_SET)
        )
    }

    /// `Utility::concatUrlPath(url, concatPath, queryItems).toEncoded()`:
    /// the path joined with [`concat_url_path`], the query replaced by
    /// `query_items` (none when empty, like an empty `QUrlQuery`). The user
    /// info is left out, as in [`Self::to_credential_free_string`].
    pub fn concat_url_path_encoded(
        &self,
        concat_path: &str,
        query_items: &[(String, String)],
    ) -> String {
        let path = concat_url_path(&self.path, concat_path);
        let mut url = format!(
            "{}://{}{}",
            self.scheme,
            self.authority,
            utf8_percent_encode(&path, PATH_ENCODE_SET)
        );
        if !query_items.is_empty() {
            url.push('?');
            url.push_str(&encode_query_items(query_items));
        }
        url
    }

    /// The URI for a decoded absolute path on this server.
    pub fn uri_for_path(&self, decoded_path: &str) -> Result<http::Uri, UrlError> {
        let encoded = utf8_percent_encode(decoded_path, PATH_ENCODE_SET).to_string();
        let s = format!("{}://{}{}", self.scheme, self.authority, encoded);
        s.parse().map_err(|_| UrlError(s))
    }
}

/// Credentials (`HttpCredentials` with a password or an app password).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl Credentials {
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            password: password.into(),
        }
    }

    /// The `Authorization` header (`Basic`), or `None` without user.
    pub fn authorization_header(&self) -> Option<HeaderValue> {
        if self.user.is_empty() {
            return None;
        }
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.user, self.password));
        HeaderValue::from_str(&format!("Basic {token}")).ok()
    }
}

/// `Account::makeServerVersion`.
pub const fn make_server_version(major: i32, minor: i32, patch: i32) -> i32 {
    (major << 16) + (minor << 8) + patch
}

/// `NEXTCLOUD_SERVER_VERSION_MOUNT_ROOT_PROPERTY_SUPPORTED_*` (VERSION.cmake).
const MOUNT_ROOT_PROPERTY_SUPPORTED: i32 = make_server_version(28, 0, 3);
/// `checksumRecalculateRequestServerVersionMinSupportedMajor`.
const CHECKSUM_RECALCULATE_MIN_MAJOR: i32 = 24;

/// `pushNotificationsReconnectInterval`: the default interval of
/// `_pushNotificationsReconnectTimer`.
pub const PUSH_NOTIFICATIONS_RECONNECT_INTERVAL: Duration = Duration::from_secs(2 * 60);

/// Logging category `nextcloud.sync.account`.
const LOG: &str = "nextcloud.sync.account";

/// `isPushNotificationsWebSocketUrlAllowed`: `wss` always, `ws` only for an
/// `http` account (no credentials in clear text for an `https` account).
pub fn is_push_notifications_web_socket_url_allowed(
    account_url: &ServerUrl,
    web_socket_url: &str,
) -> bool {
    let web_socket_scheme = web_socket_url
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .unwrap_or_default();
    if web_socket_scheme.eq_ignore_ascii_case("wss") {
        return true;
    }

    web_socket_scheme.eq_ignore_ascii_case("ws") && account_url.scheme.eq_ignore_ascii_case("http")
}

/// The push notification signals of upstream `Account`, plus every signal
/// of its `PushNotifications` object, in emission order (see
/// [`Account::subscribe_push_notifications_events`]).
#[derive(Clone, Debug)]
pub enum AccountPushEvent {
    /// `pushNotificationsReady(AccountPtr)`.
    PushNotificationsReady { account: Weak<Account> },
    /// `pushNotificationsDisabled(AccountPtr)`.
    PushNotificationsDisabled { account: Weak<Account> },
    /// A signal of [`Account::push_notifications`], forwarded after the
    /// account handled it.
    PushNotification(PushNotificationsEvent),
}

/// Room for events not yet read by a slow receiver.
const PUSH_EVENT_CHANNEL_CAPACITY: usize = 1024;

/// The push notifications part of `Account`.
#[derive(Debug)]
struct PushState {
    /// `sharedFromThis()`; `None` until [`Account::enable_push_notifications`].
    this: Option<Weak<Account>>,
    /// The options of the account's HTTP client, shared by the websocket.
    options: HttpClientOptions,
    /// `_pushNotifications`.
    push_notifications: Option<PushNotifications>,
    /// Identifies `push_notifications`, so that a deleted object's late
    /// signals are ignored (upstream disconnects them by deleting it).
    push_notifications_generation: u64,
    /// `_pushNotificationsReconnectTimer.interval()`.
    reconnect_interval: Duration,
    /// `_pushNotificationsReconnectTimer` when active: its id and task.
    reconnect_timer: Option<(u64, tokio::task::AbortHandle)>,
    reconnect_timer_generation: u64,
}

impl Default for PushState {
    fn default() -> Self {
        Self {
            this: None,
            options: HttpClientOptions::default(),
            push_notifications: None,
            push_notifications_generation: 0,
            reconnect_interval: PUSH_NOTIFICATIONS_RECONNECT_INTERVAL,
            reconnect_timer: None,
            reconnect_timer_generation: 0,
        }
    }
}

impl PushState {
    fn stop_reconnect_timer(&mut self) {
        if let Some((_, timer)) = self.reconnect_timer.take() {
            timer.abort();
        }
    }

    /// `_pushNotificationsReconnectTimer.start()`: its timeout calls
    /// `trySetupPushNotifications()`.
    fn start_reconnect_timer(&mut self) {
        self.stop_reconnect_timer();
        let Some(this) = self.this.clone() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.reconnect_timer_generation += 1;
        let generation = self.reconnect_timer_generation;
        let interval = self.reconnect_interval;
        let task = tokio::spawn(async move {
            tokio::time::sleep(interval).await;
            if let Some(account) = this.upgrade() {
                account.on_push_notifications_reconnect_timer_timeout(generation);
            }
        });
        self.reconnect_timer = Some((generation, task.abort_handle()));
    }
}

#[derive(Debug, Default)]
struct State {
    credentials: Credentials,
    dav_user: String,
    dav_display_name: String,
    server_version: String,
    capabilities: Capabilities,
}

/// The account (`Account`). Shared (`Arc`) by the engine and its jobs.
pub struct Account {
    url: ServerUrl,
    transport: Arc<dyn Transport>,
    user_agent: String,
    state: Mutex<State>,
    push: Mutex<PushState>,
    push_events: tokio::sync::broadcast::Sender<AccountPushEvent>,
    invalid_credentials: Mutex<Option<InvalidCredentialsHandler>>,
    status_reporting: Mutex<StatusReportingState>,
}

impl Drop for Account {
    fn drop(&mut self) {
        let push = self.push.get_mut().unwrap_or_else(|e| e.into_inner());
        push.stop_reconnect_timer();
        if let Some(push_notifications) = push.push_notifications.take() {
            push_notifications.close();
        }
    }
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("url", &self.url)
            .field("dav_user", &self.dav_user())
            .finish_non_exhaustive()
    }
}

/// The `User-Agent` sent with every request (`Utility::userAgentString`).
/// The `mirall/` token is what the server uses to recognise desktop clients.
pub fn default_user_agent() -> String {
    let (maj, min, patch) = nc_journal::UPSTREAM_VERSION;
    format!(
        "Mozilla/5.0 (Linux) mirall/{maj}.{min}.{patch} (nextcloud-client-rs {}, unofficial)",
        env!("CARGO_PKG_VERSION")
    )
}

impl Account {
    pub fn new(url: ServerUrl, transport: Arc<dyn Transport>) -> Self {
        let credentials = Credentials::new(url.user.clone(), url.password.clone());
        Self {
            url,
            transport,
            user_agent: default_user_agent(),
            state: Mutex::new(State {
                credentials,
                ..State::default()
            }),
            push: Mutex::new(PushState::default()),
            push_events: tokio::sync::broadcast::channel(PUSH_EVENT_CHANNEL_CAPACITY).0,
            invalid_credentials: Mutex::new(None),
            status_reporting: Mutex::new(StatusReportingState::default()),
        }
    }

    fn push(&self) -> MutexGuard<'_, PushState> {
        self.push.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn url(&self) -> &ServerUrl {
        &self.url
    }

    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    /// `setCredentials()`: also (re)starts the push notifications.
    pub fn set_credentials(&self, credentials: Credentials) {
        self.state().credentials = credentials;
        self.try_setup_push_notifications();
    }

    pub fn credentials(&self) -> Credentials {
        self.state().credentials.clone()
    }

    /// `credentials()->forgetSensitiveData()`: the password is dropped, the
    /// user is kept.
    pub fn forget_sensitive_data(&self) {
        self.state().credentials.password.clear();
    }

    /// Who is told when a request fails because of the credentials (the
    /// connections to `Account::invalidCredentials` and the app password
    /// retrieval of `handleInvalidCredentials()`). Called synchronously
    /// when the reply arrives, before the job sees it, like upstream's
    /// `AbstractNetworkJob::slotFinished`.
    pub fn set_invalid_credentials_handler(&self, handler: Option<InvalidCredentialsHandler>) {
        *self
            .invalid_credentials
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = handler;
    }

    /// `handleInvalidCredentials()`.
    pub fn handle_invalid_credentials(&self) {
        let handler = self
            .invalid_credentials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(h) = handler {
            h();
        }
    }

    /// `davUser()`: the explicit DAV user, else the credentials' user.
    pub fn dav_user(&self) -> String {
        let s = self.state();
        if s.dav_user.is_empty() {
            s.credentials.user.clone()
        } else {
            s.dav_user.clone()
        }
    }

    pub fn set_dav_user(&self, user: &str) {
        self.state().dav_user = user.to_owned();
    }

    pub fn dav_display_name(&self) -> String {
        self.state().dav_display_name.clone()
    }

    pub fn set_dav_display_name(&self, name: &str) {
        self.state().dav_display_name = name.to_owned();
    }

    pub fn server_version(&self) -> String {
        self.state().server_version.clone()
    }

    pub fn set_server_version(&self, version: &str) {
        self.state().server_version = version.to_owned();
    }

    /// `serverVersionInt()`.
    pub fn server_version_int(&self) -> i32 {
        let v = self.server_version();
        let mut parts = v.split('.');
        let mut next = || {
            parts
                .next()
                .and_then(|p| p.trim().parse::<i32>().ok())
                .unwrap_or(0)
        };
        let (a, b, c) = (next(), next(), next());
        make_server_version(a, b, c)
    }

    /// `serverHasMountRootProperty()`.
    pub fn server_has_mount_root_property(&self) -> bool {
        let v = self.server_version_int();
        v != 0 && v >= MOUNT_ROOT_PROPERTY_SUPPORTED
    }

    /// `isChecksumRecalculateRequestSupported()`.
    pub fn is_checksum_recalculate_request_supported(&self) -> bool {
        self.server_version_int() >= make_server_version(CHECKSUM_RECALCULATE_MIN_MAJOR, 0, 0)
    }

    pub fn capabilities(&self) -> Capabilities {
        self.state().capabilities.clone()
    }

    /// `setCapabilities()`: also (re)starts the push notifications.
    pub fn set_capabilities(&self, caps: Capabilities) {
        self.state().capabilities = caps;
        self.try_setup_push_notifications();
        self.try_setup_client_status_reporting();
    }

    fn status_reporting(&self) -> MutexGuard<'_, StatusReportingState> {
        self.status_reporting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Lets this account report client statuses to the server when its
    /// capabilities allow it (Rust-only switch: upstream's `Account` always
    /// does; the factory decides where the reporting database lives).
    /// Calls [`Self::try_setup_client_status_reporting`] at once, and so does
    /// [`Self::set_capabilities`] from now on.
    pub fn enable_client_status_reporting(self: &Arc<Self>, factory: ClientStatusReportingFactory) {
        self.status_reporting().factory = Some((factory, Arc::downgrade(self)));
        self.try_setup_client_status_reporting();
    }

    /// `trySetupClientStatusReporting()`.
    pub fn try_setup_client_status_reporting(&self) {
        if !self.capabilities().is_client_status_reporting_enabled() {
            self.status_reporting().reporter = None;
            return;
        }
        let factory = {
            let state = self.status_reporting();
            if state.reporter.is_some() {
                return;
            }
            state.factory.clone()
        };
        let Some((factory, this)) = factory else {
            return;
        };
        let Some(this) = this.upgrade() else {
            return;
        };
        let reporter = factory(&this);
        self.status_reporting().reporter = reporter;
    }

    /// `reportClientStatus(status)`.
    pub fn report_client_status(&self, status: ClientStatusReportingStatus) {
        let reporter = self.status_reporting().reporter.clone();
        if let Some(r) = reporter {
            r.report_client_status(status);
        }
    }

    /// Lets this account own a push notifications object, like every
    /// upstream `Account` (Rust-only switch: the one-shot `ncsync` does not
    /// call it, the daemon does). `options` are those of the account's
    /// [`crate::HttpTransport`]. Calls [`Self::try_setup_push_notifications`]
    /// at once, and so do [`Self::set_capabilities`] and
    /// [`Self::set_credentials`] from now on.
    ///
    /// Must be called inside a Tokio runtime (the websocket runs in a task).
    pub fn enable_push_notifications(self: &Arc<Self>, options: HttpClientOptions) {
        {
            let mut push = self.push();
            push.this = Some(Arc::downgrade(self));
            push.options = options;
        }
        self.try_setup_push_notifications();
    }

    /// `trySetupPushNotifications()`: creates the push notifications object
    /// if the server offers `notify_push` (rejecting a plain `ws://` endpoint
    /// for an `https` account) and (re)connects it.
    ///
    /// The account's connections to the object's signals are made at its
    /// creation, so they run before any other receiver: `ready` stops the
    /// reconnect timer and emits [`AccountPushEvent::PushNotificationsReady`];
    /// `connectionLost` and `authenticationFailed` emit
    /// [`AccountPushEvent::PushNotificationsDisabled`] when the object is not
    /// ready, and start the reconnect timer (2 minutes) if it is not running.
    pub fn try_setup_push_notifications(&self) {
        let mut push = self.push();
        let Some(this) = push.this.clone() else {
            return;
        };

        // Stop the timer to prevent parallel setup attempts
        push.stop_reconnect_timer();

        let capabilities = self.capabilities();
        if capabilities.available_push_notifications() == PushNotificationTypes::NONE {
            return;
        }
        let web_socket_url = capabilities.push_notifications_web_socket_url();
        if !is_push_notifications_web_socket_url_allowed(&self.url, &web_socket_url) {
            log::warn!(
                target: LOG,
                "Reject insecure push notifications websocket endpoint {web_socket_url} for account {}",
                self.url.to_credential_free_string()
            );
            if let Some(push_notifications) = push.push_notifications.take() {
                push_notifications.close();
            }
            drop(push);
            let _ = self
                .push_events
                .send(AccountPushEvent::PushNotificationsDisabled { account: this });
            return;
        }

        log::info!(target: LOG, "Try to setup push notifications");

        if push.push_notifications.is_none() {
            if tokio::runtime::Handle::try_current().is_err() {
                log::warn!(target: LOG, "No async runtime: push notifications are not set up");
                return;
            }
            push.push_notifications_generation += 1;
            let generation = push.push_notifications_generation;
            let account = this.clone();
            let hook = Box::new(move |event: &PushNotificationsEvent| {
                if let Some(account) = account.upgrade() {
                    account.on_push_notifications_event(generation, event);
                }
            });
            push.push_notifications = Some(PushNotifications::with_hook(
                this,
                push.options.clone(),
                Some(hook),
            ));
        }
        // If push notifications already running it is no problem to call setup again
        if let Some(push_notifications) = &push.push_notifications {
            push_notifications.setup();
        }
    }

    /// The connections of `trySetupPushNotifications()` to the object's
    /// signals, then the forwarding to [`AccountPushEvent::PushNotification`].
    fn on_push_notifications_event(&self, generation: u64, event: &PushNotificationsEvent) {
        let mut push = self.push();
        if push.push_notifications_generation != generation || push.push_notifications.is_none() {
            return;
        }
        let this = push.this.clone().unwrap_or_default();
        let mut account_event = None;
        match event {
            PushNotificationsEvent::Ready => {
                push.stop_reconnect_timer();
                account_event = Some(AccountPushEvent::PushNotificationsReady { account: this });
            }
            PushNotificationsEvent::ConnectionLost
            | PushNotificationsEvent::AuthenticationFailed => {
                log::info!(
                    target: LOG,
                    "Disable push notifications object because authentication failed or connection lost"
                );
                if push
                    .push_notifications
                    .as_ref()
                    .is_some_and(|p| !p.is_ready())
                {
                    account_event =
                        Some(AccountPushEvent::PushNotificationsDisabled { account: this });
                }
                if push.reconnect_timer.is_none() {
                    push.start_reconnect_timer();
                }
            }
            _ => {}
        }
        drop(push);
        if let Some(account_event) = account_event {
            let _ = self.push_events.send(account_event);
        }
        let _ = self
            .push_events
            .send(AccountPushEvent::PushNotification(event.clone()));
    }

    fn on_push_notifications_reconnect_timer_timeout(&self, generation: u64) {
        {
            let mut push = self.push();
            match push.reconnect_timer {
                Some((g, _)) if g == generation => push.reconnect_timer = None,
                _ => return,
            }
        }
        self.try_setup_push_notifications();
    }

    /// `pushNotifications()`: the push notifications object, if any.
    pub fn push_notifications(&self) -> Option<PushNotifications> {
        self.push().push_notifications.clone()
    }

    /// `setPushNotificationsReconnectInterval()`. Like `QTimer::setInterval`,
    /// a running timer restarts with the new interval.
    pub fn set_push_notifications_reconnect_interval(&self, interval: Duration) {
        let mut push = self.push();
        push.reconnect_interval = interval;
        if push.reconnect_timer.is_some() {
            push.start_reconnect_timer();
        }
    }

    /// Connects to `pushNotificationsReady` / `pushNotificationsDisabled`
    /// and to the signals of the push notifications object (whichever object
    /// the account owns at the time): the receiver gets every
    /// [`AccountPushEvent`] emitted from now on, in order. This is what
    /// `FolderMan::slotSetupPushNotifications` connects to.
    pub fn subscribe_push_notifications_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<AccountPushEvent> {
        self.push_events.subscribe()
    }

    /// `davPath()`: `/remote.php/dav/files/<user>/`.
    pub fn dav_path(&self) -> String {
        format!("/remote.php/dav/files/{}/", self.dav_user())
    }

    /// The decoded path of `davUrl()`.
    pub fn dav_url_path(&self) -> String {
        concat_url_path(&self.url.path, &self.dav_path())
    }

    /// `davUrl().toString()`.
    pub fn dav_url_string(&self) -> String {
        format!(
            "{}://{}{}",
            self.url.scheme,
            self.url.authority,
            utf8_percent_encode(&self.dav_url_path(), PATH_ENCODE_SET)
        )
    }

    /// `makeDavUrl(relativePath)` as a decoded path.
    pub fn dav_path_for(&self, relative_path: &str) -> String {
        concat_url_path(&self.dav_url_path(), relative_path)
    }

    /// `makeAccountUrl(relativePath)` as a decoded path.
    pub fn account_path_for(&self, relative_path: &str) -> String {
        concat_url_path(&self.url.path, relative_path)
    }

    /// The URI for a decoded absolute path.
    pub fn uri(&self, decoded_path: &str) -> Result<http::Uri, UrlError> {
        self.url.uri_for_path(decoded_path)
    }

    /// Builds a request with the headers every request carries
    /// (`AccessManager::createRequest` and `HttpCredentials`): user agent,
    /// `X-Request-ID`, authorization.
    pub fn build_request(
        &self,
        method: Method,
        uri: http::Uri,
        headers: HeaderMap,
        body: Body,
    ) -> (Request, String) {
        self.build_request_with(method, uri, headers, body, true)
    }

    /// [`Account::build_request`]; without the `Authorization` header when
    /// `add_credentials` is false (`DontAddCredentialsAttribute`).
    pub fn build_request_with(
        &self,
        method: Method,
        uri: http::Uri,
        headers: HeaderMap,
        body: Body,
        add_credentials: bool,
    ) -> (Request, String) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut req = http::Request::new(body);
        *req.method_mut() = method;
        *req.uri_mut() = uri;
        let h = req.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&self.user_agent) {
            h.insert(http::header::USER_AGENT, v);
        }
        if let Ok(v) = HeaderValue::from_str(&request_id) {
            h.insert("X-Request-ID", v);
        }
        if add_credentials && let Some(auth) = self.credentials().authorization_header() {
            h.insert(http::header::AUTHORIZATION, auth);
        }
        for (k, v) in headers.iter() {
            h.insert(k.clone(), v.clone());
        }
        (req, request_id)
    }

    /// Sends a request through the transport.
    pub async fn send(&self, request: Request) -> Result<Response, TransportError> {
        let ignore_credential_failure = request
            .extensions()
            .get::<IgnoreCredentialFailure>()
            .is_some();
        let response = self.transport.send(request).await;
        // AbstractNetworkJob::slotFinished: `!creds->stillValid(reply) &&
        // !_ignoreCredentialFailure` (WebFlowCredentials::stillValid: not
        // AuthenticationRequiredError, i.e. not a 401).
        if !ignore_credential_failure
            && response
                .as_ref()
                .is_ok_and(|r| r.status() == http::StatusCode::UNAUTHORIZED)
        {
            self.handle_invalid_credentials();
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_server_url_parsing() {
        let u = ServerUrl::parse("http://admin:se%40cret@localhost:8080/owncloud").unwrap();
        assert_eq!(u.scheme, "http");
        assert_eq!(u.authority, "localhost:8080");
        assert_eq!(u.host, "localhost");
        assert_eq!(u.path, "/owncloud");
        assert_eq!(u.user, "admin");
        assert_eq!(u.password, "se@cret");
        assert_eq!(
            u.to_credential_free_string(),
            "http://localhost:8080/owncloud"
        );
        let u = ServerUrl::parse("cloud.example.com").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.path, "");
        assert!(ServerUrl::parse("http://").is_err());
    }

    #[test]
    fn derived_concat_and_encoding() {
        assert_eq!(
            concat_url_path("/owncloud", "remote.php"),
            "/owncloud/remote.php"
        );
        assert_eq!(
            concat_url_path("/owncloud/", "/remote.php"),
            "/owncloud/remote.php"
        );
        assert_eq!(concat_url_path("", "/x"), "/x");
        let u = ServerUrl::parse("https://h/").unwrap();
        assert_eq!(
            u.uri_for_path("/a b/100%#?é").unwrap().to_string(),
            "https://h/a%20b/100%25%23%3F%C3%A9"
        );
        assert_eq!(to_percent_encoding_keep_slash("/a b/(x)"), "/a%20b/%28x%29");
    }

    #[test]
    fn derived_account_paths_and_versions() {
        struct Never;
        impl Transport for Never {
            fn send(&self, _: Request) -> crate::BoxFuture<'_, Result<Response, TransportError>> {
                Box::pin(std::future::pending())
            }
        }
        let a = Account::new(
            ServerUrl::parse("http://admin:admin@localhost/owncloud").unwrap(),
            Arc::new(Never),
        );
        assert_eq!(a.dav_user(), "admin");
        assert_eq!(a.dav_path(), "/remote.php/dav/files/admin/");
        assert_eq!(a.dav_url_path(), "/owncloud/remote.php/dav/files/admin/");
        assert_eq!(
            a.dav_path_for("/A/a1"),
            "/owncloud/remote.php/dav/files/admin/A/a1"
        );
        assert_eq!(
            a.dav_url_string(),
            "http://localhost/owncloud/remote.php/dav/files/admin/"
        );
        a.set_server_version("28.0.3");
        assert!(a.server_has_mount_root_property());
        a.set_server_version("10.0.0");
        assert!(!a.server_has_mount_root_property());
        assert_eq!(a.server_version_int(), make_server_version(10, 0, 0));
        let (req, id) = a.build_request(
            Method::GET,
            a.uri("/x").unwrap(),
            HeaderMap::new(),
            Body::empty(),
        );
        assert_eq!(req.headers()["X-Request-ID"], id.as_str());
        assert_eq!(req.headers()["authorization"], "Basic YWRtaW46YWRtaW4=");
    }
}
