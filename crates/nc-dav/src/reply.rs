// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2015 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the error handling parts of upstream `src/libsync/abstractnetworkjob.cpp`
// (nextcloud/desktop v34.0.5) and of Qt's `QNetworkReply::NetworkError`.

//! The outcome of a network request, as upstream jobs see a finished
//! `QNetworkReply`: an HTTP status (or 0), a `QNetworkReply::NetworkError`
//! code, headers and the body.

use bytes::Bytes;
use http::HeaderMap;

use crate::transport::TransportError;

/// `QNetworkReply::NetworkError`, with Qt's numeric values (upstream compares
/// ranges, e.g. `nerror <= UnknownProxyError` means "network error").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum NetworkError {
    #[default]
    NoError = 0,
    ConnectionRefusedError = 1,
    RemoteHostClosedError = 2,
    HostNotFoundError = 3,
    TimeoutError = 4,
    OperationCanceledError = 5,
    SslHandshakeFailedError = 6,
    TemporaryNetworkFailureError = 7,
    NetworkSessionFailedError = 8,
    BackgroundRequestNotAllowedError = 9,
    TooManyRedirectsError = 10,
    InsecureRedirectError = 11,
    UnknownNetworkError = 99,
    ProxyConnectionRefusedError = 101,
    ProxyConnectionClosedError = 102,
    ProxyNotFoundError = 103,
    ProxyTimeoutError = 104,
    ProxyAuthenticationRequiredError = 105,
    UnknownProxyError = 199,
    ContentAccessDenied = 201,
    ContentOperationNotPermittedError = 202,
    ContentNotFoundError = 203,
    AuthenticationRequiredError = 204,
    ContentReSendError = 205,
    ContentConflictError = 206,
    ContentGoneError = 207,
    UnknownContentError = 299,
    ProtocolUnknownError = 301,
    ProtocolInvalidOperationError = 302,
    ProtocolFailure = 399,
    InternalServerError = 401,
    OperationNotImplementedError = 402,
    ServiceUnavailableError = 403,
    UnknownServerError = 499,
}

impl NetworkError {
    /// Qt's `statusCodeFromHttp()`: the error code a `QNetworkReply` reports
    /// for an HTTP status >= 400 (`NoError` below 400).
    pub fn from_http_status(status: u16) -> Self {
        if status < 400 {
            return Self::NoError;
        }
        match status {
            400 => Self::ProtocolInvalidOperationError,
            401 => Self::AuthenticationRequiredError,
            403 => Self::ContentAccessDenied,
            404 => Self::ContentNotFoundError,
            405 => Self::ContentOperationNotPermittedError,
            407 => Self::ProxyAuthenticationRequiredError,
            409 => Self::ContentConflictError,
            410 => Self::ContentGoneError,
            418 => Self::ProtocolInvalidOperationError,
            500 => Self::InternalServerError,
            501 => Self::OperationNotImplementedError,
            503 => Self::ServiceUnavailableError,
            s if s > 500 => Self::UnknownServerError,
            _ => Self::UnknownContentError,
        }
    }

    /// The error code for a failure below HTTP.
    pub fn from_transport(e: &TransportError) -> Self {
        match e {
            TransportError::Canceled => Self::OperationCanceledError,
            TransportError::Timeout => Self::TimeoutError,
            TransportError::RemoteHostClosed(_) => Self::RemoteHostClosedError,
            TransportError::ConnectionRefused(_) => Self::ConnectionRefusedError,
            TransportError::HostNotFound(_) => Self::HostNotFoundError,
            TransportError::SslHandshakeFailed(_) => Self::SslHandshakeFailedError,
            TransportError::Connection(_) | TransportError::Other(_) => Self::UnknownNetworkError,
        }
    }

    /// The enumerator name, as `QMetaEnum::valueToKey` prints it.
    pub fn name(self) -> &'static str {
        match self {
            Self::NoError => "NoError",
            Self::ConnectionRefusedError => "ConnectionRefusedError",
            Self::RemoteHostClosedError => "RemoteHostClosedError",
            Self::HostNotFoundError => "HostNotFoundError",
            Self::TimeoutError => "TimeoutError",
            Self::OperationCanceledError => "OperationCanceledError",
            Self::SslHandshakeFailedError => "SslHandshakeFailedError",
            Self::TemporaryNetworkFailureError => "TemporaryNetworkFailureError",
            Self::NetworkSessionFailedError => "NetworkSessionFailedError",
            Self::BackgroundRequestNotAllowedError => "BackgroundRequestNotAllowedError",
            Self::TooManyRedirectsError => "TooManyRedirectsError",
            Self::InsecureRedirectError => "InsecureRedirectError",
            Self::UnknownNetworkError => "UnknownNetworkError",
            Self::ProxyConnectionRefusedError => "ProxyConnectionRefusedError",
            Self::ProxyConnectionClosedError => "ProxyConnectionClosedError",
            Self::ProxyNotFoundError => "ProxyNotFoundError",
            Self::ProxyTimeoutError => "ProxyTimeoutError",
            Self::ProxyAuthenticationRequiredError => "ProxyAuthenticationRequiredError",
            Self::UnknownProxyError => "UnknownProxyError",
            Self::ContentAccessDenied => "ContentAccessDenied",
            Self::ContentOperationNotPermittedError => "ContentOperationNotPermittedError",
            Self::ContentNotFoundError => "ContentNotFoundError",
            Self::AuthenticationRequiredError => "AuthenticationRequiredError",
            Self::ContentReSendError => "ContentReSendError",
            Self::ContentConflictError => "ContentConflictError",
            Self::ContentGoneError => "ContentGoneError",
            Self::UnknownContentError => "UnknownContentError",
            Self::ProtocolUnknownError => "ProtocolUnknownError",
            Self::ProtocolInvalidOperationError => "ProtocolInvalidOperationError",
            Self::ProtocolFailure => "ProtocolFailure",
            Self::InternalServerError => "InternalServerError",
            Self::OperationNotImplementedError => "OperationNotImplementedError",
            Self::ServiceUnavailableError => "ServiceUnavailableError",
            Self::UnknownServerError => "UnknownServerError",
        }
    }
}

/// Lets a transport dictate the `QNetworkReply` error of a response instead
/// of deriving it from the HTTP status. Stored in the response extensions.
///
/// Real servers never need this. The FakeFolder harness uses it to reproduce
/// upstream's fake replies exactly: `FakeErrorReply` reports
/// `InternalServerError` whatever its status, `FakeGetReply` on a missing file
/// reports `ContentNotFoundError` without any HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyOverride {
    pub error: NetworkError,
    /// `None` means the reply has no HTTP status attribute (0).
    pub http_status: Option<u16>,
}

/// A finished request, the equivalent of a finished `QNetworkReply` plus the
/// job state upstream keeps next to it (`_timedout`).
#[derive(Clone, Debug, Default)]
pub struct Reply {
    /// The request URL.
    pub url: String,
    /// `HttpStatusCodeAttribute` (0 when there was no HTTP response).
    pub http_status: u16,
    /// `HttpReasonPhraseAttribute`.
    pub reason_phrase: String,
    pub error: NetworkError,
    /// Description of a transport failure (`QNetworkReply::errorString`).
    pub transport_error: String,
    pub headers: HeaderMap,
    /// The whole body (empty for streamed downloads).
    pub body: Bytes,
    /// The job timed out (`AbstractNetworkJob::_timedout`).
    pub timed_out: bool,
    /// The `X-Request-ID` header sent with the request.
    pub request_id: String,
}

impl Reply {
    /// A reply for a request that failed below HTTP.
    pub fn from_transport_error(url: String, request_id: String, e: &TransportError) -> Self {
        Self {
            url,
            request_id,
            error: NetworkError::from_transport(e),
            transport_error: e.to_string(),
            timed_out: matches!(e, TransportError::Timeout),
            ..Self::default()
        }
    }

    /// Fills status, reason, error and headers from a response head.
    pub fn from_parts(url: String, request_id: String, parts: &http::response::Parts) -> Self {
        let mut reply = Self {
            url,
            request_id,
            http_status: parts.status.as_u16(),
            reason_phrase: parts.status.canonical_reason().unwrap_or("").to_owned(),
            error: NetworkError::from_http_status(parts.status.as_u16()),
            headers: parts.headers.clone(),
            ..Self::default()
        };
        if let Some(o) = parts.extensions.get::<ReplyOverride>() {
            reply.error = o.error;
            reply.http_status = o.http_status.unwrap_or(0);
        }
        reply
    }

    pub fn is_ok(&self) -> bool {
        self.error == NetworkError::NoError
    }

    /// `rawHeader(name)` as bytes (empty when missing).
    pub fn raw_header(&self, name: &str) -> &[u8] {
        self.headers
            .get(name)
            .map(|v| v.as_bytes())
            .unwrap_or_default()
    }

    /// `rawHeader(name)` as a string (lossy).
    pub fn raw_header_str(&self, name: &str) -> String {
        String::from_utf8_lossy(self.raw_header(name)).into_owned()
    }

    pub fn has_raw_header(&self, name: &str) -> bool {
        self.headers.contains_key(name)
    }

    /// `AbstractNetworkJob::responseTimestamp()`: the `Date` header.
    pub fn response_timestamp(&self) -> Vec<u8> {
        self.raw_header("Date").to_vec()
    }

    /// `QNetworkReply::header(ContentTypeHeader)`.
    pub fn content_type(&self) -> String {
        self.raw_header_str("Content-Type")
    }

    /// `AbstractNetworkJob::errorString()`.
    pub fn error_string(&self) -> String {
        if self.timed_out {
            return "The server took too long to respond. Check your connection and try syncing again. If it still doesn’t work, reach out to your server administrator.".to_owned();
        }
        if self.has_raw_header("OC-ErrorString") {
            return self.raw_header_str("OC-ErrorString");
        }
        network_reply_error_string(self.http_status).to_owned()
    }

    /// `AbstractNetworkJob::errorStringParsingBody()`: the `<s:message>` of a
    /// Sabre error body when there is one, else [`Reply::error_string`].
    pub fn error_string_parsing_body(&self) -> String {
        let error_message = self.error_string();
        if error_message.is_empty() {
            return String::new();
        }
        let extra = crate::xml::extract_error_message(&self.body);
        if !extra.is_empty() && !self.has_raw_header("OC-ErrorString") {
            return extra;
        }
        error_message
    }

    /// `AbstractNetworkJob::errorStringParsingBodyException()`.
    pub fn error_string_parsing_body_exception(&self) -> String {
        crate::xml::extract_exception(&self.body)
    }

    /// `replyStatusString()` (for logs).
    pub fn status_string(&self) -> String {
        if self.is_ok() {
            "OK".to_owned()
        } else {
            format!("{} {}", self.error.name(), self.error_string())
        }
    }
}

/// `networkReplyErrorString()`: the user friendly message for an HTTP status.
pub fn network_reply_error_string(http_status: u16) -> &'static str {
    match http_status {
        400 => {
            "We couldn’t process your request. Please try syncing again later. If this keeps happening, contact your server administrator for help."
        }
        401 => {
            "You need to sign in to continue. If you have trouble with your credentials, please reach out to your server administrator."
        }
        403 => {
            "You don’t have access to this resource. If you think this is a mistake, please contact your server administrator."
        }
        404 => {
            "We couldn’t find what you were looking for. It might have been moved or deleted. If you need help, contact your server administrator."
        }
        407 => {
            "It seems you are using a proxy that required authentication. Please check your proxy settings and credentials. If you need help, contact your server administrator."
        }
        408 => {
            "The request is taking longer than usual. Please try syncing again. If it still doesn’t work, reach out to your server administrator."
        }
        409 => {
            "Server files changed while you were working. Please try syncing again. Contact your server administrator if the issue persists."
        }
        410 => {
            "This folder or file isn’t available anymore. If you need assistance, please contact your server administrator."
        }
        412 => {
            "The request could not be completed because some required conditions were not met. Please try syncing again later. If you need assistance, please contact your server administrator."
        }
        413 => {
            "The file is too big to upload. You might need to choose a smaller file or contact your server administrator for assistance."
        }
        414 => {
            "The address used to make the request is too long for the server to handle. Please try shortening the information you’re sending or contact your server administrator for assistance."
        }
        415 => {
            "This file type isn’t supported. Please contact your server administrator for assistance."
        }
        422 => {
            "The server couldn’t process your request because some information was incorrect or incomplete. Please try syncing again later, or contact your server administrator for assistance."
        }
        423 => {
            "The resource you are trying to access is currently locked and cannot be modified. Please try changing it later, or contact your server administrator for assistance."
        }
        428 => {
            "This request could not be completed because it is missing some required conditions. Please try again later, or contact your server administrator for help."
        }
        429 => {
            "You made too many requests. Please wait and try again. If you keep seeing this, your server administrator can help."
        }
        500 => {
            "Something went wrong on the server. Please try syncing again later, or contact your server administrator if the issue persists."
        }
        501 => {
            "The server does not recognize the request method. Please contact your server administrator for help."
        }
        502 => {
            "We’re having trouble connecting to the server. Please try again soon. If the issue persists, your server administrator can help you."
        }
        503 => {
            "The server is busy right now. Please try connecting again in a few minutes or contact your server administrator if it’s urgent."
        }
        504 => {
            "It’s taking too long to connect to the server. Please try again later. If you need help, contact your server administrator."
        }
        505 => {
            "The server does not support the version of the connection being used. Contact your server administrator for help."
        }
        507 => {
            "The server does not have enough space to complete your request. Please check how much quota your user has by contacting your server administrator."
        }
        511 => {
            "Your network needs extra authentication. Please check your connection. Contact your server administrator for help if the issue persists."
        }
        513 => {
            "You don’t have permission to access this resource. If you believe this is an error, contact your server administrator to ask for assistance."
        }
        _ => {
            "An unexpected error occurred. Please try syncing again or contact your server administrator if the issue continues."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_mapping_matches_qt() {
        use NetworkError::*;
        assert_eq!(NetworkError::from_http_status(207), NoError);
        assert_eq!(NetworkError::from_http_status(404), ContentNotFoundError);
        assert_eq!(NetworkError::from_http_status(412), UnknownContentError);
        assert_eq!(NetworkError::from_http_status(423), UnknownContentError);
        assert_eq!(NetworkError::from_http_status(500), InternalServerError);
        assert_eq!(NetworkError::from_http_status(507), UnknownServerError);
        // Network errors come before content errors in Qt's numbering.
        assert!(OperationCanceledError <= UnknownProxyError);
        assert!(ContentNotFoundError > UnknownProxyError);
    }

    #[test]
    fn error_string_prefers_oc_error_string_header() {
        let mut r = Reply {
            http_status: 403,
            error: NetworkError::ContentAccessDenied,
            ..Reply::default()
        };
        assert!(r.error_string().starts_with("You don’t have access"));
        r.headers
            .insert("OC-ErrorString", "Quota exceeded".parse().unwrap());
        assert_eq!(r.error_string(), "Quota exceeded");
        r.timed_out = true;
        assert!(r.error_string().starts_with("The server took too long"));
    }
}
