// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `nc-dav`: the HTTP / WebDAV / OCS layer of the (unofficial) Rust port of
//! the Nextcloud desktop client (upstream `src/libsync` network jobs,
//! account, capabilities, credentials, push notifications).
//!
//! Phase 0 only contains the [`transport`] abstraction: every request the
//! future sync engine makes goes through a [`Transport`], so the engine can
//! run against a real server (a `reqwest` implementation, Phase 1) or
//! in-process against the FakeFolder harness (`nc-testutils`), which plays
//! the role of upstream's `FakeQNAM` (a `QNetworkAccessManager` subclass).

pub mod transport;

pub use transport::{Body, BoxFuture, Request, Response, Transport, TransportError, method};
