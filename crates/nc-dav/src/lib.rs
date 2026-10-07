// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! `nc-dav`: the HTTP / WebDAV / OCS layer of the (unofficial) Rust port of
//! the Nextcloud desktop client (upstream `src/libsync` network jobs,
//! account, capabilities, credentials).
//!
//! Every request goes through a [`Transport`]: [`HttpTransport`] (reqwest)
//! for a real server, or the in-process fake server of `nc-testutils`, which
//! plays the role of upstream's `FakeQNAM`.

// A failed request is an ordinary outcome carrying the whole reply (status,
// headers, body), like a finished `QNetworkReply`; boxing it would only add
// noise at every call site.
#![allow(clippy::result_large_err)]

pub mod account;
pub mod capabilities;
pub mod decompress;
pub mod http_client;
pub mod jobs;
pub mod push_notifications;
pub mod reply;
pub mod transport;
pub mod xml;

pub use account::{Account, AccountPushEvent, Credentials, ServerUrl};
pub use capabilities::Capabilities;
pub use http_client::{HttpClientOptions, HttpTransport};
pub use jobs::{HttpError, JobOptions, Target};
pub use push_notifications::{PushNotifications, PushNotificationsEvent};
pub use reply::{NetworkError, Reply, ReplyOverride};
pub use transport::{
    Body, BodyStream, BoxFuture, Bytes, DecompressedSafetyCheckThreshold, DecompressionError,
    Request, Response, Transport, TransportError, method,
};
