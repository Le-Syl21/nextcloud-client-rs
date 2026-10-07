// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/accountstate.{h,cpp}` (nextcloud/desktop
// v34.0.5).

//! `AccountState`: the connection state machine of one account.
//!
//! The account is checked when it is created and then every
//! [`DEFAULT_CALLING_INTERVAL`] (62 s), with a back-off of one interval per
//! failed attempt (up to 8), unless push notifications are ready. While
//! disconnected, `index.php/204` is probed on the same interval to notice
//! the server coming back.
//!
//! Signals are returned as [`AccountSignal`]s for the daemon to dispatch
//! (`stateChanged`, `isConnectedChanged`, `termsOfServiceChanged`, the push
//! notification setup). Not ported: navigation apps, desktop notification
//! settings and user status, remote wipe, the unbranded→branded migration.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use nc_dav::Account;
use tokio::sync::mpsc::UnboundedSender;

use crate::connection_validator::{
    ConnectionValidator, DEFAULT_CALLING_INTERVAL, Status as ConnectionStatus, ValidationResult,
};
use crate::timer::{Timer, single_shot};

const LOG: &str = "nextcloud.gui.account.state";

/// `AccountState::State`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not even attempting to connect, most likely because the
    /// user explicitly signed out or cancelled a credential dialog.
    SignedOut,
    /// Account would like to be connected but hasn't heard back yet.
    Disconnected,
    /// The account is successfully talking to the server.
    Connected,
    /// There's a temporary problem with talking to the server,
    /// don't bother the user too much and try again.
    ServiceUnavailable,
    /// Connection is being redirected (likely a captive portal is in effect)
    /// Do not proceed with connecting and check back later
    RedirectDetected,
    /// Similar to ServiceUnavailable, but we know the server is down
    /// for maintenance
    MaintenanceMode,
    /// Could not communicate with the server for some reason.
    /// We assume this may resolve itself over time and will try
    /// again automatically.
    NetworkError,
    /// Server configuration error. (For example: unsupported version)
    ConfigurationError,
    /// We are currently asking the user for credentials
    AskingCredentials,
    /// Need to sign terms of service by going to web UI
    NeedToSignTermsOfService,
}

impl State {
    /// `stateString(state)`.
    pub fn state_string(self) -> &'static str {
        match self {
            Self::SignedOut => "Signed out",
            Self::Disconnected => "Disconnected",
            Self::Connected => "Connected",
            Self::ServiceUnavailable => "Service unavailable",
            Self::MaintenanceMode => "Maintenance mode",
            Self::RedirectDetected => "Redirect detected",
            Self::NetworkError => "Network error",
            Self::ConfigurationError => "Configuration error",
            Self::AskingCredentials => "Asking Credentials",
            Self::NeedToSignTermsOfService => "Need the user to accept the terms of service",
        }
    }

    /// The `Q_ENUM` key (JSON output).
    pub fn name(self) -> &'static str {
        match self {
            Self::SignedOut => "SignedOut",
            Self::Disconnected => "Disconnected",
            Self::Connected => "Connected",
            Self::ServiceUnavailable => "ServiceUnavailable",
            Self::RedirectDetected => "RedirectDetected",
            Self::MaintenanceMode => "MaintenanceMode",
            Self::NetworkError => "NetworkError",
            Self::ConfigurationError => "ConfigurationError",
            Self::AskingCredentials => "AskingCredentials",
            Self::NeedToSignTermsOfService => "NeedToSignTermsOfService",
        }
    }
}

/// What the account state asks the daemon to do or to tell others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountSignal {
    /// `stateChanged(state)` (emitted even when only the errors changed).
    StateChanged(State),
    /// `isConnectedChanged()`.
    IsConnectedChanged,
    /// `termsOfServiceChanged(account, state)`.
    TermsOfServiceChanged(State),
    /// `account()->trySetupPushNotifications()`.
    TrySetupPushNotifications,
    /// The credentials are missing or were refused: upstream asks the user
    /// (`askFromUser()`); a daemon cannot, it logs how to fix it.
    CredentialsNeeded,
}

/// The events an account state posts to the daemon's queue.
#[derive(Debug)]
pub enum AccountEvent {
    /// `_checkConnectionTimer` timeout.
    CheckConnectionTimer(u64),
    /// `_checkServerAvailibilityTimer` timeout.
    CheckServerAvailabilityTimer(u64),
    /// The `index.php/204` probe answered 204.
    ServerAvailable,
    /// `QTimer::singleShot(_maintenanceToConnectedDelay + 100, checkConnectivity)`.
    CheckConnectivity,
    /// `connectionResult(status, errors)` of the running validator.
    ConnectionResult(ValidationResult),
}

/// `AccountState`.
pub struct AccountState {
    id: String,
    account: Arc<Account>,
    state: State,
    connection_status: ConnectionStatus,
    connection_errors: Vec<String>,
    waiting_for_new_credentials: bool,
    time_of_last_etag_check: Option<SystemTime>,
    validator_running: bool,
    time_since_maintenance_over: Option<Instant>,
    /// 1-5 min, random.
    maintenance_to_connected_delay: Duration,
    retry_count: u32,
    last_check_connection_timer: Option<Instant>,
    last_connection_validator_status: ConnectionStatus,
    /// `credentials()->wasFetched()`.
    credentials_fetched: bool,
    /// `credentials()->ready()`.
    credentials_ready: bool,
    /// `pushNotifications() && pushNotifications()->isReady()`.
    push_notifications_ready: bool,
    check_connection_timer: Timer,
    check_server_availability_timer: Timer,
    /// The remote poll interval (`ConfigFile::remotePollInterval`).
    remote_poll_interval: Duration,
}

/// Wraps an [`AccountEvent`] into the daemon's event type.
pub type EventWrapper<E> = Arc<dyn Fn(String, AccountEvent) -> E>;

impl AccountState {
    /// `AccountState(account)`. `credentials_ready`: an app password is
    /// known (`fetchFromKeychain` succeeded).
    pub fn new(
        id: &str,
        account: Arc<Account>,
        credentials_ready: bool,
        remote_poll_interval: Duration,
    ) -> Self {
        Self {
            id: id.to_owned(),
            account,
            state: State::Disconnected,
            connection_status: ConnectionStatus::Undefined,
            connection_errors: Vec::new(),
            waiting_for_new_credentials: false,
            time_of_last_etag_check: None,
            validator_running: false,
            time_since_maintenance_over: None,
            maintenance_to_connected_delay: Duration::from_millis(
                60_000 + fastrand::u64(0..4 * 60_000),
            ),
            retry_count: 0,
            last_check_connection_timer: None,
            last_connection_validator_status: ConnectionStatus::Undefined,
            credentials_fetched: true,
            credentials_ready,
            push_notifications_ready: false,
            check_connection_timer: Timer::new(DEFAULT_CALLING_INTERVAL, false),
            check_server_availability_timer: Timer::new(DEFAULT_CALLING_INTERVAL, false),
            remote_poll_interval,
        }
    }

    /// An account state that is connected and never checks its connection
    /// (the `FakeAccountState` of upstream's tests).
    pub fn new_fake_connected(id: &str, account: Arc<Account>) -> Self {
        let mut s = Self::new(id, account, true, Duration::from_secs(30));
        s.state = State::Connected;
        s.connection_status = ConnectionStatus::Connected;
        s
    }

    /// Starts the timers and posts the first connection check (the end of
    /// upstream's constructor).
    pub fn start<E: 'static>(&mut self, tx: &UnboundedSender<E>, wrap: EventWrapper<E>) {
        let (w1, id1) = (wrap.clone(), self.id.clone());
        self.check_connection_timer.start(tx, move |g| {
            w1(id1.clone(), AccountEvent::CheckConnectionTimer(g))
        });
        let (w2, id2) = (wrap.clone(), self.id.clone());
        self.check_server_availability_timer.start(tx, move |g| {
            w2(id2.clone(), AccountEvent::CheckServerAvailabilityTimer(g))
        });
        let g = self.check_connection_timer.generation();
        let _ = tx.send(wrap(self.id.clone(), AccountEvent::CheckConnectionTimer(g)));
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn account(&self) -> &Arc<Account> {
        &self.account
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn connection_status(&self) -> ConnectionStatus {
        self.connection_status
    }

    pub fn connection_errors(&self) -> &[String] {
        &self.connection_errors
    }

    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    pub fn is_signed_out(&self) -> bool {
        self.state == State::SignedOut
    }

    pub fn needs_to_sign_terms_of_service(&self) -> bool {
        self.state == State::NeedToSignTermsOfService
    }

    /// `tagLastSuccessfullETagRequest(tp)`.
    pub fn tag_last_successful_etag_request(&mut self, tp: SystemTime) {
        self.time_of_last_etag_check = Some(tp);
    }

    /// Push notifications became ready or went away.
    pub fn set_push_notifications_ready(&mut self, ready: bool) {
        self.push_notifications_ready = ready;
    }

    /// New credentials were given (e.g. after `ncsync account login`):
    /// `slotCredentialsAsked` with ready credentials.
    pub fn set_credentials_ready<E: 'static>(
        &mut self,
        ready: bool,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
    ) -> Vec<AccountSignal> {
        self.credentials_ready = ready;
        self.waiting_for_new_credentials = false;
        let mut signals = Vec::new();
        if !ready {
            // User canceled the connection or did not give a password
            self.set_state(State::SignedOut, tx, wrap, &mut signals);
            return signals;
        }
        if self.state == State::SignedOut || self.state == State::AskingCredentials {
            self.set_state(State::Disconnected, tx, wrap, &mut signals);
        }
        // When new credentials become available we always want to restart the
        // connection validation, even if it's currently running.
        self.validator_running = false;
        self.check_connectivity(tx, wrap, &mut signals);
        signals
    }

    fn reset_retry_count(&mut self) {
        self.retry_count = 0;
    }

    fn increase_retry_count(&mut self) {
        self.retry_count += 1;
    }

    /// `setState(state)`.
    fn set_state<E: 'static>(
        &mut self,
        state: State,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
        signals: &mut Vec<AccountSignal>,
    ) {
        if self.state != state {
            log::info!(target: LOG, "AccountState state change:  {} -> {}", self.state.state_string(), state.state_string());
            let old_state = self.state;
            self.state = state;
            if self.state == State::SignedOut {
                self.connection_status = ConnectionStatus::Undefined;
                if old_state == State::Disconnected {
                    log::info!(target: LOG, "Invalid credentials for {} login needed", self.account.url().to_credential_free_string());
                    self.set_state(State::AskingCredentials, tx, wrap, signals);
                }
            } else if old_state == State::SignedOut && self.state == State::Disconnected {
                // If we stop being voluntarily signed-out, try to connect and
                // auth right now!
                self.check_connectivity(tx, wrap, signals);
            } else if self.state == State::ServiceUnavailable
                || self.state == State::RedirectDetected
            {
                // Check if we are actually down for maintenance/in a redirect state (captive portal?).
                // To do this we must clear the connection validator that just
                // produced the 503/302. It's finished anyway and will delete itself.
                self.validator_running = false;
                self.check_connectivity(tx, wrap, signals);
            }
            if old_state == State::Connected || self.state == State::Connected {
                signals.push(AccountSignal::IsConnectedChanged);
            }
            if self.state == State::Connected {
                self.reset_retry_count();
            }
        }
        // might not have changed but the underlying _connectionErrors might have
        signals.push(AccountSignal::StateChanged(self.state));
    }

    /// `checkConnectivity()`.
    pub fn check_connectivity<E: 'static>(
        &mut self,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
        signals: &mut Vec<AccountSignal>,
    ) {
        log::info!(target: LOG, "check connectivity");
        if self.is_signed_out() || self.waiting_for_new_credentials {
            return;
        }
        if self.validator_running {
            log::warn!(target: LOG, "ConnectionValidator already running, ignoring {}", self.account.dav_display_name());
            return;
        }
        // If we never fetched credentials, do that now - otherwise connection attempts
        // make little sense, we might be missing client certs.
        if !self.credentials_fetched {
            self.credentials_fetched = true;
            signals.push(AccountSignal::CredentialsNeeded);
        }
        // IF the account is connected the connection check can be skipped
        // if the last successful etag check job is not so long ago.
        let polltime = self.remote_poll_interval.as_secs();
        if self.is_connected()
            && let Some(t) = self.time_of_last_etag_check
        {
            let elapsed = SystemTime::now()
                .duration_since(t)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if elapsed <= polltime {
                log::debug!(target: LOG, "{} The last ETag check succeeded within the last {polltime}s ({elapsed}s). No connection check needed!", self.account.dav_display_name());
                return;
            }
        }
        self.validator_running = true;
        let previous_errors = std::mem::take(&mut self.connection_errors);
        let account = self.account.clone();
        let credentials_ready = self.credentials_ready;
        let only_auth = self.is_connected() || self.needs_to_sign_terms_of_service();
        let tx = tx.clone();
        let wrap = wrap.clone();
        let id = self.id.clone();
        tokio::task::spawn_local(async move {
            let validator = ConnectionValidator::new(&account, credentials_ready);
            let result = if only_auth {
                // Use a small authed propfind as a minimal ping when we're
                // already connected.
                validator.check_authentication(&previous_errors).await
            } else {
                // Check the server and then the auth.
                validator.check_server_and_auth(&previous_errors).await
            };
            let _ = tx.send(wrap(id, AccountEvent::ConnectionResult(result)));
        });
    }

    /// Handles an event posted by this account state.
    pub fn handle_event<E: 'static>(
        &mut self,
        event: AccountEvent,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
    ) -> Vec<AccountSignal> {
        let mut signals = Vec::new();
        match event {
            AccountEvent::CheckConnectionTimer(g) => {
                // The initial `QTimer::singleShot(0, slotCheckConnection)`
                // carries the timer's current generation without firing it.
                if g == self.check_connection_timer.generation() {
                    let _ = self.check_connection_timer.fired(g);
                    self.slot_check_connection(tx, wrap, &mut signals);
                }
            }
            AccountEvent::CheckServerAvailabilityTimer(g) => {
                if self.check_server_availability_timer.fired(g) {
                    self.slot_check_server_availability(tx, wrap);
                }
            }
            AccountEvent::ServerAvailable => {
                log::info!(target: LOG, "Server is now available for account {}", self.account.dav_user());
                self.last_check_connection_timer = None;
                self.reset_retry_count();
                self.slot_check_connection(tx, wrap, &mut signals);
            }
            AccountEvent::CheckConnectivity => self.check_connectivity(tx, wrap, &mut signals),
            AccountEvent::ConnectionResult(result) => {
                self.validator_running = false;
                self.slot_connection_validator_result(
                    result.status,
                    result.errors,
                    tx,
                    wrap,
                    &mut signals,
                );
            }
        }
        signals
    }

    /// `slotConnectionValidatorResult(status, errors)`.
    fn slot_connection_validator_result<E: 'static>(
        &mut self,
        status: ConnectionStatus,
        errors: Vec<String>,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
        signals: &mut Vec<AccountSignal>,
    ) {
        if self.is_signed_out() {
            log::warn!(target: LOG, "Signed out, ignoring {} {}", status.name(), self.account.url().to_credential_free_string());
            return;
        }
        let old_connection_validator_status = self.last_connection_validator_status;
        self.last_connection_validator_status = status;
        // Come online gradually from 503, captive portal(redirection) or maintenance mode
        if status == ConnectionStatus::Connected
            && matches!(
                self.connection_status,
                ConnectionStatus::ServiceUnavailable
                    | ConnectionStatus::MaintenanceMode
                    | ConnectionStatus::StatusRedirect
            )
        {
            match self.time_since_maintenance_over {
                None => {
                    log::info!(target: LOG, "AccountState reconnection: delaying for {} ms", self.maintenance_to_connected_delay.as_millis());
                    self.time_since_maintenance_over = Some(Instant::now());
                    single_shot(
                        self.maintenance_to_connected_delay + Duration::from_millis(100),
                        tx,
                        wrap(self.id.clone(), AccountEvent::CheckConnectivity),
                    );
                    return;
                }
                Some(t) if t.elapsed() < self.maintenance_to_connected_delay => {
                    log::info!(target: LOG, "AccountState reconnection: only {} ms have passed", t.elapsed().as_millis());
                    return;
                }
                Some(_) => {}
            }
        }
        if self.connection_status != status {
            log::info!(target: LOG, "AccountState connection status change:  {} -> {}", self.connection_status.name(), status.name());
            self.connection_status = status;
            signals.push(AccountSignal::StateChanged(self.state));
        }
        self.connection_errors = errors;
        let update_retry_count = |s: &mut Self| {
            s.increase_retry_count();
            log::info!(target: LOG, "connection retry count {}", s.retry_count);
            s.last_check_connection_timer = Some(Instant::now());
        };
        match status {
            ConnectionStatus::Connected => {
                if self.state != State::Connected {
                    self.set_state(State::Connected, tx, wrap, signals);
                    // resetRetryConnection
                    log::info!(target: LOG, "reset retry count");
                    self.reset_retry_count();
                    self.last_check_connection_timer = Some(Instant::now());
                    // Setup push notifications after a successful connection
                    signals.push(AccountSignal::TrySetupPushNotifications);
                }
            }
            ConnectionStatus::Undefined | ConnectionStatus::NotConfigured => {
                self.set_state(State::Disconnected, tx, wrap, signals);
                update_retry_count(self);
            }
            ConnectionStatus::ServerVersionMismatch => {
                self.set_state(State::ConfigurationError, tx, wrap, signals);
            }
            ConnectionStatus::StatusNotFound => {
                // This can happen either because the server does not exist
                // or because we are having network issues. The latter one is
                // much more likely, so keep trying to connect.
                self.set_state(State::NetworkError, tx, wrap, signals);
                update_retry_count(self);
            }
            ConnectionStatus::CredentialsWrong | ConnectionStatus::CredentialsNotReady => {
                self.handle_invalid_credentials(tx, wrap, signals);
            }
            ConnectionStatus::SslError => self.set_state(State::SignedOut, tx, wrap, signals),
            ConnectionStatus::ServiceUnavailable => {
                self.time_since_maintenance_over = None;
                self.set_state(State::ServiceUnavailable, tx, wrap, signals);
            }
            ConnectionStatus::MaintenanceMode => {
                self.time_since_maintenance_over = None;
                self.set_state(State::MaintenanceMode, tx, wrap, signals);
            }
            ConnectionStatus::StatusRedirect => {
                self.time_since_maintenance_over = None;
                self.set_state(State::RedirectDetected, tx, wrap, signals);
            }
            ConnectionStatus::Timeout => {
                self.set_state(State::NetworkError, tx, wrap, signals);
                update_retry_count(self);
            }
            ConnectionStatus::NeedToSignTermsOfService => {
                self.set_state(State::NeedToSignTermsOfService, tx, wrap, signals);
            }
        }
        if (old_connection_validator_status == ConnectionStatus::NeedToSignTermsOfService
            && status == ConnectionStatus::Connected)
            || (status == ConnectionStatus::NeedToSignTermsOfService
                && old_connection_validator_status != status)
        {
            signals.push(AccountSignal::TermsOfServiceChanged(
                if status == ConnectionStatus::NeedToSignTermsOfService {
                    State::NeedToSignTermsOfService
                } else {
                    State::Connected
                },
            ));
        }
    }

    /// `handleInvalidCredentials()`.
    fn handle_invalid_credentials<E: 'static>(
        &mut self,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
        signals: &mut Vec<AccountSignal>,
    ) {
        if self.is_signed_out() || self.waiting_for_new_credentials {
            return;
        }
        log::info!(target: LOG, "Invalid credentials for {} asking user", self.account.url().to_credential_free_string());
        self.waiting_for_new_credentials = true;
        self.set_state(State::AskingCredentials, tx, wrap, signals);
        // askFromUser(): nobody to ask in a daemon.
        signals.push(AccountSignal::CredentialsNeeded);
    }

    /// `slotCheckConnection()`.
    fn slot_check_connection<E: 'static>(
        &mut self,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
        signals: &mut Vec<AccountSignal>,
    ) {
        if let Some(last) = self.last_check_connection_timer {
            let interval = DEFAULT_CALLING_INTERVAL.as_millis() as u64;
            let max = interval * 8;
            let min_delay = (u64::from(self.retry_count) * interval).max(interval);
            let current_delay = min_delay.min(max);
            let elapsed = last.elapsed().as_millis() as u64;
            // !hasExpired(currentDelay - 1)
            if elapsed <= current_delay.saturating_sub(1) {
                log::info!(target: LOG, "timer has not expired: do not check now {elapsed} {current_delay}");
                return;
            }
        }
        let current_state = self.state;
        // Don't check if we're manually signed out or
        // when the error is permanent.
        if current_state != State::SignedOut
            && current_state != State::ConfigurationError
            && current_state != State::AskingCredentials
            && !self.push_notifications_ready
        {
            self.check_connectivity(tx, wrap, signals);
        } else if current_state == State::SignedOut
            && self.last_connection_validator_status == ConnectionStatus::SslError
        {
            log::warn!(target: LOG, "Account is signed out due to SSL Handshake error. Going to perform a sign-in attempt...");
            // trySignIn() → signIn()
            self.waiting_for_new_credentials = false;
            self.set_state(State::Disconnected, tx, wrap, signals);
        }
    }

    /// `slotCheckServerAvailibility()`.
    fn slot_check_server_availability<E: 'static>(
        &mut self,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
    ) {
        if matches!(
            self.state,
            State::Connected | State::SignedOut | State::MaintenanceMode | State::AskingCredentials
        ) {
            log::info!(target: LOG, "Skipping server availability check for account {} with state {}", self.account.dav_user(), self.state.name());
            return;
        }
        log::info!(target: LOG, "Checking server availability for account {}", self.account.dav_user());
        let account = self.account.clone();
        let tx = tx.clone();
        let wrap = wrap.clone();
        let id = self.id.clone();
        tokio::task::spawn_local(async move {
            let reply = nc_dav::jobs::send(
                &account,
                http::Method::GET,
                &nc_dav::Target::Account("index.php/204".to_owned()),
                http::HeaderMap::new(),
                nc_dav::Body::empty(),
                &nc_dav::JobOptions::default(),
            )
            .await;
            if reply.http_status == 204 {
                let _ = tx.send(wrap(id, AccountEvent::ServerAvailable));
            }
        });
    }

    /// `slotPushNotificationsReady()`.
    pub fn slot_push_notifications_ready<E: 'static>(
        &mut self,
        tx: &UnboundedSender<E>,
        wrap: &EventWrapper<E>,
    ) -> Vec<AccountSignal> {
        let mut signals = Vec::new();
        self.push_notifications_ready = true;
        if self.state != State::Connected {
            self.set_state(State::Connected, tx, wrap, &mut signals);
        }
        signals
    }
}
