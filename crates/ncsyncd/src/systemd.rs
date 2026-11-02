// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! What only the headless daemon does around the shared event loop:
//! systemd's `Type=notify` protocol (`READY`, `STATUS`, `RELOADING`,
//! `STOPPING`), its watchdog, and the Unix signals.
//!
//! The watchdog is pinged from the event loop itself (a timer posting
//! [`Event::WatchdogTick`] to the queue at half of `WatchdogSec`), so a
//! stuck loop gets the service restarted.

use std::time::Duration;

use nc_daemon::daemon::{DaemonHooks, ReloadResult};
use nc_daemon::event::Event;
use nc_daemon::folder_man::{FolderMan, ManagerRequest};
use nc_daemon::startup::ReloadHooks;
use nc_daemon::timer::Timer;
use tokio::sync::mpsc::UnboundedSender;

const LOG: &str = "nextcloud.daemon";

/// systemd notification (no-op outside systemd).
fn notify(state: &[sd_notify::NotifyState]) {
    let _ = sd_notify::notify(state);
}

/// Posts [`Event::Shutdown`] on SIGTERM/SIGINT and [`Event::Reload`] on
/// SIGHUP (`systemctl reload`). Needs a `LocalSet`.
pub fn install_signal_handlers(tx: &UnboundedSender<Event>) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let tx = tx.clone();
    tokio::task::spawn_local(async move {
        loop {
            let event = tokio::select! {
                _ = term.recv() => Event::Shutdown,
                _ = int.recv() => Event::Shutdown,
                _ = hup.recv() => Event::Reload,
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// The hooks of `ncsyncd`: the configuration hooks of [`ReloadHooks`],
/// plus systemd.
pub struct SystemdHooks {
    pub inner: ReloadHooks,
    watchdog: Timer,
}

impl SystemdHooks {
    pub fn new(inner: ReloadHooks) -> Self {
        Self {
            inner,
            watchdog: Timer::new(Duration::ZERO, false),
        }
    }
}

impl DaemonHooks for SystemdHooks {
    fn started(&mut self, tx: &UnboundedSender<Event>) {
        if let Some(interval) = sd_notify::watchdog_enabled() {
            log::info!(target: LOG, "systemd watchdog every {} ms", interval.as_millis());
            self.watchdog
                .start_with(interval / 2, tx, |_| Event::WatchdogTick);
        }
        notify(&[sd_notify::NotifyState::Ready]);
        self.inner.started(tx);
    }

    fn watchdog(&mut self) {
        notify(&[sd_notify::NotifyState::Watchdog]);
    }

    fn status_changed(&mut self, status: &str) {
        notify(&[sd_notify::NotifyState::Status(status)]);
    }

    fn reloading(&mut self) {
        let mut state = vec![sd_notify::NotifyState::Reloading];
        if let Ok(now) = sd_notify::NotifyState::monotonic_usec_now() {
            state.push(now);
        }
        notify(&state);
    }

    fn reloaded(&mut self) {
        notify(&[sd_notify::NotifyState::Ready]);
    }

    fn stopping(&mut self) {
        notify(&[sd_notify::NotifyState::Stopping]);
    }

    fn stopped(&mut self) {
        self.watchdog.stop();
    }

    fn reload(
        &mut self,
        fm: &mut FolderMan,
        config: Option<&str>,
        wipe: &[String],
    ) -> ReloadResult {
        self.inner.reload(fm, config, wipe)
    }

    fn manager_request(&mut self, fm: &mut FolderMan, request: ManagerRequest) {
        match &request {
            // askFromUser(): there is nobody to ask. The STATUS line says
            // which accounts need to sign in (see `status_line`).
            ManagerRequest::CredentialsNeeded(id) => {
                log::error!(target: LOG, "Account {id} needs new credentials: run `ncsync account add <server_url>` (or provide the systemd credential); a running ncsyncd picks them up");
            }
        }
        self.inner.manager_request(fm, request);
    }

    fn control(
        &mut self,
        fm: &mut FolderMan,
        request: nc_daemon::control::Request,
        tx: &UnboundedSender<Event>,
        reply: tokio::sync::oneshot::Sender<nc_daemon::control::Response>,
    ) {
        self.inner.control(fm, request, tx, reply);
    }
}
