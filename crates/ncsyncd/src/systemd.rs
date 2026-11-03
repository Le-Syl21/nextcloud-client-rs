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

#[cfg(unix)]
use sd_notify as sd;

/// Without systemd (Windows) every notification is a no-op.
#[cfg(not(unix))]
#[allow(dead_code)]
mod sd {
    pub enum NotifyState<'a> {
        Ready,
        Watchdog,
        Status(&'a str),
        Reloading,
        Stopping,
    }

    impl NotifyState<'_> {
        pub fn monotonic_usec_now() -> std::io::Result<NotifyState<'static>> {
            Err(std::io::Error::other("no systemd"))
        }
    }

    pub fn notify(_state: &[NotifyState<'_>]) -> std::io::Result<()> {
        Ok(())
    }

    pub fn watchdog_enabled() -> Option<std::time::Duration> {
        None
    }
}

/// systemd notification (no-op outside systemd).
fn notify(state: &[sd::NotifyState]) {
    let _ = sd::notify(state);
}

/// Posts [`Event::Shutdown`] on SIGTERM/SIGINT and [`Event::Reload`] on
/// SIGHUP (`systemctl reload`). Needs a `LocalSet`.
#[cfg(unix)]
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

/// Windows: Ctrl+C, Ctrl+Break, closing the console, logging off and
/// shutting down post [`Event::Shutdown`]; there is no signal to reload (the
/// control endpoint's `reload` request does it). Needs a `LocalSet`.
#[cfg(windows)]
pub fn install_signal_handlers(tx: &UnboundedSender<Event>) -> std::io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
    let mut c = ctrl_c()?;
    let mut brk = ctrl_break()?;
    let mut close = ctrl_close()?;
    let mut logoff = ctrl_logoff()?;
    let mut shutdown = ctrl_shutdown()?;
    let tx = tx.clone();
    tokio::task::spawn_local(async move {
        tokio::select! {
            _ = c.recv() => {}
            _ = brk.recv() => {}
            _ = close.recv() => {}
            _ = logoff.recv() => {}
            _ = shutdown.recv() => {}
        }
        let _ = tx.send(Event::Shutdown);
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
        if let Some(interval) = sd::watchdog_enabled() {
            log::info!(target: LOG, "systemd watchdog every {} ms", interval.as_millis());
            self.watchdog
                .start_with(interval / 2, tx, |_| Event::WatchdogTick);
        }
        notify(&[sd::NotifyState::Ready]);
        self.inner.started(tx);
    }

    fn watchdog(&mut self) {
        notify(&[sd::NotifyState::Watchdog]);
    }

    fn status_changed(&mut self, status: &str) {
        notify(&[sd::NotifyState::Status(status)]);
    }

    fn reloading(&mut self) {
        let mut state = vec![sd::NotifyState::Reloading];
        if let Ok(now) = sd::NotifyState::monotonic_usec_now() {
            state.push(now);
        }
        notify(&state);
    }

    fn reloaded(&mut self) {
        notify(&[sd::NotifyState::Ready]);
    }

    fn stopping(&mut self) {
        notify(&[sd::NotifyState::Stopping]);
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
