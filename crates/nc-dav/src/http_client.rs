// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The real [`Transport`]: a `reqwest` client (rustls, HTTP/2, cookies),
//! the counterpart of upstream's `AccessManager`/`QNetworkAccessManager`.
//!
//! Like `QNetworkAccessManager`, the transport asks for and transparently
//! decodes gzip/deflate bodies, with Qt's decompression safety check (see
//! [`crate::decompress`]); reqwest's own decoding is not used because it
//! has no such check.

use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};

use crate::decompress::{self, Decompressor};
use crate::transport::{
    Body, BodyStream, BoxFuture, DecompressedSafetyCheckThreshold, Request, Response, Transport,
    TransportError,
};

/// Options of the HTTP client.
#[derive(Clone, Debug, Default)]
pub struct HttpClientOptions {
    /// Accept invalid TLS certificates (`nextcloudcmd --trust`).
    pub trust_invalid_certificates: bool,
    /// An explicit HTTP proxy (`--httpproxy http://host:port`).
    pub proxy: Option<String>,
}

/// A `reqwest`-based transport.
#[derive(Clone, Debug)]
pub struct HttpTransport {
    client: reqwest::Client,
}

fn map_error(e: &reqwest::Error) -> TransportError {
    let msg = e.to_string();
    if e.is_timeout() {
        return TransportError::Timeout;
    }
    if e.is_connect() {
        // Walk the source chain for a precise cause.
        let mut source: Option<&dyn std::error::Error> = std::error::Error::source(e);
        while let Some(s) = source {
            let text = s.to_string().to_lowercase();
            if text.contains("refused") {
                return TransportError::ConnectionRefused(msg);
            }
            if text.contains("dns") || text.contains("resolve") || text.contains("name or service")
            {
                return TransportError::HostNotFound(msg);
            }
            if text.contains("certificate") || text.contains("tls") || text.contains("handshake") {
                return TransportError::SslHandshakeFailed(msg);
            }
            source = s.source();
        }
        return TransportError::Connection(msg);
    }
    if e.is_body() || e.is_decode() {
        return TransportError::RemoteHostClosed(msg);
    }
    TransportError::Other(msg)
}

impl HttpTransport {
    pub fn new(options: &HttpClientOptions) -> Result<Self, TransportError> {
        let mut builder = reqwest::Client::builder()
            .cookie_store(true)
            .danger_accept_invalid_certs(options.trust_invalid_certificates)
            // Upstream does not follow HTTPS -> HTTP downgrades.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let downgrade = attempt.previous().last().is_some_and(|prev| {
                    prev.scheme() == "https" && attempt.url().scheme() == "http"
                });
                if downgrade || attempt.previous().len() >= 10 {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }));
        if let Some(p) = &options.proxy {
            let proxy = reqwest::Proxy::all(p).map_err(|e| TransportError::Other(e.to_string()))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|e| TransportError::Other(e.to_string()))?;
        Ok(Self { client })
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        Box::pin(async move {
            let (mut parts, body) = request.into_parts();
            let url = parts.uri.to_string();
            let threshold = parts
                .extensions
                .get::<DecompressedSafetyCheckThreshold>()
                .map_or(
                    decompress::DEFAULT_DECOMPRESSED_SAFETY_CHECK_THRESHOLD,
                    |t| t.0,
                );
            // Qt decompresses only when the caller did not ask for an
            // encoding itself.
            let auto_decompress = !parts.headers.contains_key(http::header::ACCEPT_ENCODING);
            if auto_decompress {
                parts.headers.insert(
                    http::header::ACCEPT_ENCODING,
                    http::HeaderValue::from_static(decompress::ACCEPTED_ENCODINGS),
                );
            }
            let mut builder = self
                .client
                .request(parts.method, url)
                .headers(parts.headers);
            builder = match body {
                Body::Full(b) => builder.body(b),
                Body::Stream { stream, .. } => builder.body(reqwest::Body::wrap_stream(stream)),
            };
            let resp = builder.send().await.map_err(|e| map_error(&e))?;
            let status = resp.status();
            let version = resp.version();
            let mut headers = resp.headers().clone();
            let mut len = resp.content_length();
            let stream: BodyStream = Box::pin(
                resp.bytes_stream()
                    .map_err(|e| std::io::Error::other(e.to_string())),
            );
            let compressed = auto_decompress
                && headers
                    .get(http::header::CONTENT_ENCODING)
                    .is_some_and(|v| decompress::is_supported_encoding(v.as_bytes()));
            let stream = if compressed {
                // Qt removes the Content-Length of a transparently decompressed
                // HTTP/1 reply, not of an HTTP/2 one (QTBUG-73364).
                if version != http::Version::HTTP_2 {
                    headers.remove(http::header::CONTENT_LENGTH);
                }
                len = None;
                decompressing_stream(stream, Decompressor::new(threshold))
            } else {
                stream
            };
            let mut out = http::Response::new(Body::from_stream(stream, len));
            *out.status_mut() = status;
            *out.version_mut() = version;
            *out.headers_mut() = headers;
            Ok(out)
        })
    }
}

/// Decodes a compressed body stream as it arrives.
fn decompressing_stream(inner: BodyStream, decompressor: Decompressor) -> BodyStream {
    struct State {
        inner: BodyStream,
        decompressor: Decompressor,
        pending: VecDeque<Bytes>,
        failed: bool,
    }
    let state = State {
        inner,
        decompressor,
        pending: VecDeque::new(),
        failed: false,
    };
    Box::pin(futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(piece) = st.pending.pop_front() {
                return Some((Ok(piece), st));
            }
            if st.failed {
                return None;
            }
            match st.inner.next().await {
                Some(Ok(chunk)) => match st.decompressor.feed(&chunk) {
                    Ok(pieces) => st.pending.extend(pieces),
                    Err(e) => {
                        st.failed = true;
                        return Some((Err(e.into_io()), st));
                    }
                },
                Some(Err(e)) => {
                    st.failed = true;
                    return Some((Err(e), st));
                }
                None => return None,
            }
        }
    }))
}
