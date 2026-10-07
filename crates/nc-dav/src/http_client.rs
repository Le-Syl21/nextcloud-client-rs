// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The real [`Transport`]: a `reqwest` client (rustls, HTTP/2, cookies,
//! gzip/deflate decoding), the counterpart of upstream's
//! `AccessManager`/`QNetworkAccessManager`.

use futures_util::TryStreamExt;

use crate::transport::{Body, BoxFuture, Request, Response, Transport, TransportError};

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
            let (parts, body) = request.into_parts();
            let url = parts.uri.to_string();
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
            let headers = resp.headers().clone();
            let len = resp.content_length();
            let stream = resp
                .bytes_stream()
                .map_err(|e| std::io::Error::other(e.to_string()));
            let mut out = http::Response::new(Body::from_stream(stream, len));
            *out.status_mut() = status;
            *out.headers_mut() = headers;
            Ok(out)
        })
    }
}
