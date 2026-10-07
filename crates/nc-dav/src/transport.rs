// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! HTTP transport abstraction.
//!
//! Upstream routes all traffic through `QNetworkAccessManager::createRequest`
//! and tests replace the manager with `FakeQNAM`. The Rust equivalent is the
//! [`Transport`] trait, implemented by [`crate::HttpTransport`] (reqwest) for
//! real servers and by the in-process fake server of `nc-testutils`.
//!
//! # Design choices
//!
//! - Requests and responses are [`http`] crate types. The body is a
//!   [`Body`]: either buffered ([`Body::Full`]) or a stream of chunks
//!   ([`Body::Stream`]) so that uploads read the file while sending (like
//!   upstream's `UploadDevice`) and downloads write it while receiving (like
//!   `GETFileJob::slotReadyRead`). WebDAV verbs (PROPFIND, MKCOL, MOVE...)
//!   are extension methods, see [`method`].
//! - [`Transport::send`] returns a boxed `Send` future instead of using
//!   `async fn` in the trait, so that the trait stays dyn-compatible
//!   (`Arc<dyn Transport>` held by the account) without the `async-trait`
//!   crate. An in-process implementation can return an already completed
//!   future; a request that never completes (upstream `FakeHangingReply`) is
//!   `std::future::pending()`.
//! - Errors that are not HTTP statuses (upstream `QNetworkReply` error codes
//!   like `OperationCanceledError` or a connection failure) are
//!   [`TransportError`]. Any HTTP status, including 4xx/5xx, is an `Ok`
//!   [`Response`]: interpreting statuses is the job layer's business, as in
//!   upstream `AbstractNetworkJob`.

use std::future::Future;
use std::pin::Pin;

pub use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// A stream of body chunks.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// Request and response body.
pub enum Body {
    /// A body held in memory.
    Full(Bytes),
    /// A body produced chunk by chunk. `len` is the total length when known
    /// (sent as `Content-Length`).
    Stream {
        stream: BodyStream,
        len: Option<u64>,
    },
}

impl Default for Body {
    fn default() -> Self {
        Self::Full(Bytes::new())
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(b) => f.debug_tuple("Full").field(&b.len()).finish(),
            Self::Stream { len, .. } => f.debug_struct("Stream").field("len", len).finish(),
        }
    }
}

impl From<Bytes> for Body {
    fn from(b: Bytes) -> Self {
        Self::Full(b)
    }
}

impl From<Vec<u8>> for Body {
    fn from(b: Vec<u8>) -> Self {
        Self::Full(Bytes::from(b))
    }
}

impl From<&'static str> for Body {
    fn from(s: &'static str) -> Self {
        Self::Full(Bytes::from_static(s.as_bytes()))
    }
}

impl From<String> for Body {
    fn from(s: String) -> Self {
        Self::Full(Bytes::from(s))
    }
}

impl Body {
    pub fn empty() -> Self {
        Self::default()
    }

    /// A streaming body from any chunk stream.
    pub fn from_stream(
        stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
        len: Option<u64>,
    ) -> Self {
        Self::Stream {
            stream: Box::pin(stream),
            len,
        }
    }

    /// The buffered bytes, if this is a [`Body::Full`].
    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            Self::Full(b) => Some(b),
            Self::Stream { .. } => None,
        }
    }

    /// The length when known.
    pub fn len_hint(&self) -> Option<u64> {
        match self {
            Self::Full(b) => Some(b.len() as u64),
            Self::Stream { len, .. } => *len,
        }
    }

    /// Reads the whole body into memory.
    pub async fn collect(self) -> Result<Bytes, std::io::Error> {
        match self {
            Self::Full(b) => Ok(b),
            Self::Stream { mut stream, len } => {
                let mut out = Vec::with_capacity(len.unwrap_or(0).min(1 << 20) as usize);
                while let Some(chunk) = stream.next().await {
                    out.extend_from_slice(&chunk?);
                }
                Ok(Bytes::from(out))
            }
        }
    }

    /// Converts into a chunk stream (a [`Body::Full`] yields one chunk).
    pub fn into_stream(self) -> BodyStream {
        match self {
            Self::Full(b) => {
                if b.is_empty() {
                    Box::pin(futures_util::stream::empty())
                } else {
                    Box::pin(futures_util::stream::once(std::future::ready(Ok(b))))
                }
            }
            Self::Stream { stream, .. } => stream,
        }
    }
}

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
    /// The server closed the connection (`RemoteHostClosedError`).
    #[error("connection closed: {0}")]
    RemoteHostClosed(String),
    /// The server refused the connection (`ConnectionRefusedError`).
    #[error("connection refused: {0}")]
    ConnectionRefused(String),
    /// Name resolution failed (`HostNotFoundError`).
    #[error("host not found: {0}")]
    HostNotFound(String),
    /// TLS handshake failure (`SslHandshakeFailedError`).
    #[error("TLS handshake failed: {0}")]
    SslHandshakeFailed(String),
    /// Other connection-level failure (`UnknownNetworkError`).
    #[error("connection error: {0}")]
    Connection(String),
    /// The body could not be decoded (`UnknownContentError`), e.g. the
    /// decompression safety check of a compressed reply.
    #[error("{0}")]
    UnknownContent(String),
    /// Anything else, with a human-readable message.
    #[error("{0}")]
    Other(String),
}

/// The error a body stream yields (wrapped in a [`std::io::Error`]) when the
/// transparent decompression of a reply fails (`UnknownContentError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DecompressionError(pub String);

impl DecompressionError {
    /// Wraps the error for a body stream.
    pub fn into_io(self) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, self)
    }
}

/// A request extension: `QNetworkRequest::setDecompressedSafetyCheckThreshold`
/// (bytes; `-1` disables the check). Without it the transport uses Qt's
/// default, [`crate::decompress::DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecompressedSafetyCheckThreshold(pub i64);

/// Maps a body stream error: a failed decompression is
/// [`TransportError::UnknownContent`], anything else a closed connection.
pub fn body_stream_error(e: &std::io::Error) -> TransportError {
    if let Some(d) = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DecompressionError>())
    {
        return TransportError::UnknownContent(d.0.clone());
    }
    TransportError::RemoteHostClosed(e.to_string())
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
    pub const PATCH: Method = Method::PATCH;

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
            let body = Body::from(request.method().as_str().to_owned());
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
            .body(Body::empty())
            .unwrap();
        let resp = block_on(t.send(req)).unwrap();
        assert_eq!(
            block_on(resp.into_body().collect()).unwrap().as_ref(),
            b"PROPFIND"
        );
    }

    #[test]
    fn streaming_body_collects_in_order() {
        let chunks = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
        let body = Body::from_stream(futures_util::stream::iter(chunks), Some(4));
        assert_eq!(body.len_hint(), Some(4));
        assert_eq!(block_on(body.collect()).unwrap().as_ref(), b"abcd");
    }
}
