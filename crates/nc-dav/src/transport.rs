// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! In-process HTTP transport abstraction.
//!
//! Upstream routes all traffic through `QNetworkAccessManager::createRequest`
//! and tests replace the manager with `FakeQNAM`. The Rust equivalent is the
//! [`Transport`] trait.
//!
//! # Design choices
//!
//! - Requests and responses are [`http`] crate types with a buffered
//!   [`Bytes`] body. `reqwest` and `hyper` speak `http` natively, so the real
//!   implementation is a thin adapter. WebDAV verbs (PROPFIND, MKCOL, MOVE,
//!   ...) are extension methods, see [`method`].
//! - [`Transport::send`] returns a boxed `Send` future instead of using
//!   `async fn` in the trait: the trait stays dyn-compatible
//!   (`Arc<dyn Transport>` held by the account and shared by the propagator
//!   jobs running on a multi-threaded tokio runtime) without pulling in the
//!   `async-trait` crate. An in-process implementation simply returns
//!   `Box::pin(std::future::ready(..))`; a request that never completes
//!   (upstream `FakeHangingReply`) is `std::future::pending()`.
//! - Errors that are not HTTP statuses (upstream `QNetworkReply` error codes
//!   like `OperationCanceledError` or a connection failure) are
//!   [`TransportError`]. Any HTTP status, including 4xx/5xx, is an `Ok`
//!   [`Response`]: interpreting statuses is the job layer's business, as in
//!   upstream `AbstractNetworkJob`.
//! - Open point for Phase 1: large downloads/uploads need streaming bodies;
//!   the body type will then become an enum or a stream, with `Bytes` kept as
//!   the buffered variant.

use std::future::Future;
use std::pin::Pin;

pub use bytes::Bytes;

/// Request and response body (buffered).
pub type Body = Bytes;
/// An HTTP request (method, URI, headers, body).
pub type Request = http::Request<Body>;
/// An HTTP response (status, headers, body).
pub type Response = http::Response<Body>;
/// Boxed future returned by [`Transport::send`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Failures below the HTTP layer (`QNetworkReply::NetworkError` other than
/// the HTTP status mapped ones).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The request was aborted (`OperationCanceledError`).
    #[error("operation canceled")]
    Canceled,
    /// The request timed out (`TimeoutError`).
    #[error("timeout")]
    Timeout,
    /// Connection-level failure (DNS, TCP, TLS...).
    #[error("connection error: {0}")]
    Connection(String),
    /// Anything else, with a human-readable message.
    #[error("{0}")]
    Other(String),
}

/// Sends HTTP requests. Implemented by the real HTTP client and by the fake
/// in-process server of `nc-testutils`.
pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>>;
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        (**self).send(request)
    }
}

/// HTTP and WebDAV methods used by the client. The standard ones are
/// re-exported from [`http::Method`]; the WebDAV extensions are built with
/// `Method::from_bytes` (infallible for these tokens).
pub mod method {
    use http::Method;

    pub use http::Method as Any;

    pub const GET: Method = Method::GET;
    pub const PUT: Method = Method::PUT;
    pub const POST: Method = Method::POST;
    pub const DELETE: Method = Method::DELETE;
    pub const HEAD: Method = Method::HEAD;
    pub const OPTIONS: Method = Method::OPTIONS;

    fn ext(name: &'static str) -> Method {
        Method::from_bytes(name.as_bytes()).expect("valid HTTP token")
    }

    pub fn propfind() -> Method {
        ext("PROPFIND")
    }
    pub fn proppatch() -> Method {
        ext("PROPPATCH")
    }
    pub fn mkcol() -> Method {
        ext("MKCOL")
    }
    pub fn move_() -> Method {
        ext("MOVE")
    }
    pub fn copy() -> Method {
        ext("COPY")
    }
    pub fn lock() -> Method {
        ext("LOCK")
    }
    pub fn unlock() -> Method {
        ext("UNLOCK")
    }
    pub fn report() -> Method {
        ext("REPORT")
    }
    pub fn search() -> Method {
        ext("SEARCH")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Transport for Echo {
        fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
            let body = Bytes::from(request.method().as_str().to_owned());
            Box::pin(std::future::ready(Ok(Response::new(body))))
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("not ready"),
        }
    }

    #[test]
    fn dyn_transport_with_webdav_method() {
        let t: std::sync::Arc<dyn Transport> = std::sync::Arc::new(Echo);
        let req = http::Request::builder()
            .method(method::propfind())
            .uri("https://example.org/remote.php/dav/files/u/")
            .body(Bytes::new())
            .unwrap();
        let resp = block_on(t.send(req)).unwrap();
        assert_eq!(resp.body().as_ref(), b"PROPFIND");
    }
}
