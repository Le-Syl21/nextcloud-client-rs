// SPDX-FileCopyrightText: 2016 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2013 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the parts of upstream `src/libsync/account.cpp` and
// `src/libsync/creds/httpcredentials.cpp` (nextcloud/desktop v34.0.5) the sync
// engine and `nextcloudcmd` need.

//! The account: server URL, credentials, user, server version and
//! capabilities, plus the transport every request goes through.

use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use http::{HeaderMap, HeaderValue, Method};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use crate::capabilities::Capabilities;
use crate::transport::{Body, Request, Response, Transport, TransportError};

/// Characters `QUrl` percent-encodes in a path when sending it.
pub const PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Characters `QUrl::toPercentEncoding(s, "/")` leaves alone: unreserved
/// (`ALPHA DIGIT - . _ ~`) plus `/`.
pub const TO_PERCENT_ENCODING_SLASH: &AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// `QUrl::toPercentEncoding(s, "/")`.
pub fn to_percent_encoding_keep_slash(s: &str) -> String {
    utf8_percent_encode(s, TO_PERCENT_ENCODING_SLASH).to_string()
}

/// `Utility::concatUrlPath` on paths: joins avoiding `//` and a missing `/`.
pub fn concat_url_path(path: &str, concat_path: &str) -> String {
    let mut path = path.to_owned();
    if !concat_path.is_empty() {
        if path.ends_with('/') && concat_path.starts_with('/') {
            path.pop();
        } else if !path.ends_with('/') && !concat_path.starts_with('/') {
            path.push('/');
        }
        path.push_str(concat_path);
    }
    path
}

/// A server URL split like `QUrl`: scheme, authority (without user info),
/// decoded path, and the user info.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerUrl {
    pub scheme: String,
    /// `host[:port]`.
    pub authority: String,
    pub host: String,
    /// Decoded path, e.g. `/owncloud` (may be empty).
    pub path: String,
    pub user: String,
    pub password: String,
}

/// Errors parsing a server URL.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid server URL: {0}")]
pub struct UrlError(pub String);

impl ServerUrl {
    /// Parses `scheme://[user[:password]@]host[:port][/path]`. A URL without
    /// scheme gets `https://` (`QUrl::fromUserInput`).
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        let input = input.trim();
        let (scheme, rest) = match input.split_once("://") {
            Some((s, r)) => (s.to_ascii_lowercase(), r),
            None => ("https".to_owned(), input),
        };
        if scheme.is_empty() {
            return Err(UrlError(input.to_owned()));
        }
        let (authority_full, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (userinfo, authority) = match authority_full.rsplit_once('@') {
            Some((u, a)) => (Some(u), a),
            None => (None, authority_full),
        };
        if authority.is_empty() {
            return Err(UrlError(input.to_owned()));
        }
        // QUrl normalizes the host to lower case.
        let authority = authority.to_ascii_lowercase();
        let authority = authority.as_str();
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((u, p)) => (decode(u), decode(p)),
                None => (decode(u), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let host = if authority.starts_with('[') {
            authority
                .split(']')
                .next()
                .map(|h| format!("{h}]"))
                .unwrap_or_default()
        } else {
            authority.split(':').next().unwrap_or("").to_owned()
        };
        // Strip query and fragment.
        let path = path.split(['?', '#']).next().unwrap_or("");
        Ok(Self {
            scheme,
            authority: authority.to_owned(),
            host,
            path: decode(path),
            user,
            password,
        })
    }

    /// The URL without user info (`credentialFreeUrl`), as text.
    pub fn to_credential_free_string(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme,
            self.authority,
            utf8_percent_encode(&self.path, PATH_ENCODE_SET)
        )
    }

    /// The URI for a decoded absolute path on this server.
    pub fn uri_for_path(&self, decoded_path: &str) -> Result<http::Uri, UrlError> {
        let encoded = utf8_percent_encode(decoded_path, PATH_ENCODE_SET).to_string();
        let s = format!("{}://{}{}", self.scheme, self.authority, encoded);
        s.parse().map_err(|_| UrlError(s))
    }
}

/// Credentials (`HttpCredentials` with a password or an app password).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl Credentials {
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            password: password.into(),
        }
    }

    /// The `Authorization` header (`Basic`), or `None` without user.
    pub fn authorization_header(&self) -> Option<HeaderValue> {
        if self.user.is_empty() {
            return None;
        }
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.user, self.password));
        HeaderValue::from_str(&format!("Basic {token}")).ok()
    }
}

/// `Account::makeServerVersion`.
pub const fn make_server_version(major: i32, minor: i32, patch: i32) -> i32 {
    (major << 16) + (minor << 8) + patch
}

/// `NEXTCLOUD_SERVER_VERSION_MOUNT_ROOT_PROPERTY_SUPPORTED_*` (VERSION.cmake).
const MOUNT_ROOT_PROPERTY_SUPPORTED: i32 = make_server_version(28, 0, 3);
/// `checksumRecalculateRequestServerVersionMinSupportedMajor`.
const CHECKSUM_RECALCULATE_MIN_MAJOR: i32 = 24;

#[derive(Debug, Default)]
struct State {
    credentials: Credentials,
    dav_user: String,
    dav_display_name: String,
    server_version: String,
    capabilities: Capabilities,
}

/// The account (`Account`). Shared (`Arc`) by the engine and its jobs.
pub struct Account {
    url: ServerUrl,
    transport: Arc<dyn Transport>,
    user_agent: String,
    state: Mutex<State>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("url", &self.url)
            .field("dav_user", &self.dav_user())
            .finish_non_exhaustive()
    }
}

/// The `User-Agent` sent with every request (`Utility::userAgentString`).
/// The `mirall/` token is what the server uses to recognise desktop clients.
pub fn default_user_agent() -> String {
    let (maj, min, patch) = nc_journal::UPSTREAM_VERSION;
    format!(
        "Mozilla/5.0 (Linux) mirall/{maj}.{min}.{patch} (nextcloud-client-rs {}, unofficial)",
        env!("CARGO_PKG_VERSION")
    )
}

impl Account {
    pub fn new(url: ServerUrl, transport: Arc<dyn Transport>) -> Self {
        let credentials = Credentials::new(url.user.clone(), url.password.clone());
        Self {
            url,
            transport,
            user_agent: default_user_agent(),
            state: Mutex::new(State {
                credentials,
                ..State::default()
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn url(&self) -> &ServerUrl {
        &self.url
    }

    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    pub fn set_credentials(&self, credentials: Credentials) {
        self.state().credentials = credentials;
    }

    pub fn credentials(&self) -> Credentials {
        self.state().credentials.clone()
    }

    /// `davUser()`: the explicit DAV user, else the credentials' user.
    pub fn dav_user(&self) -> String {
        let s = self.state();
        if s.dav_user.is_empty() {
            s.credentials.user.clone()
        } else {
            s.dav_user.clone()
        }
    }

    pub fn set_dav_user(&self, user: &str) {
        self.state().dav_user = user.to_owned();
    }

    pub fn dav_display_name(&self) -> String {
        self.state().dav_display_name.clone()
    }

    pub fn set_dav_display_name(&self, name: &str) {
        self.state().dav_display_name = name.to_owned();
    }

    pub fn server_version(&self) -> String {
        self.state().server_version.clone()
    }

    pub fn set_server_version(&self, version: &str) {
        self.state().server_version = version.to_owned();
    }

    /// `serverVersionInt()`.
    pub fn server_version_int(&self) -> i32 {
        let v = self.server_version();
        let mut parts = v.split('.');
        let mut next = || {
            parts
                .next()
                .and_then(|p| p.trim().parse::<i32>().ok())
                .unwrap_or(0)
        };
        let (a, b, c) = (next(), next(), next());
        make_server_version(a, b, c)
    }

    /// `serverHasMountRootProperty()`.
    pub fn server_has_mount_root_property(&self) -> bool {
        let v = self.server_version_int();
        v != 0 && v >= MOUNT_ROOT_PROPERTY_SUPPORTED
    }

    /// `isChecksumRecalculateRequestSupported()`.
    pub fn is_checksum_recalculate_request_supported(&self) -> bool {
        self.server_version_int() >= make_server_version(CHECKSUM_RECALCULATE_MIN_MAJOR, 0, 0)
    }

    pub fn capabilities(&self) -> Capabilities {
        self.state().capabilities.clone()
    }

    pub fn set_capabilities(&self, caps: Capabilities) {
        self.state().capabilities = caps;
    }

    /// `davPath()`: `/remote.php/dav/files/<user>/`.
    pub fn dav_path(&self) -> String {
        format!("/remote.php/dav/files/{}/", self.dav_user())
    }

    /// The decoded path of `davUrl()`.
    pub fn dav_url_path(&self) -> String {
        concat_url_path(&self.url.path, &self.dav_path())
    }

    /// `davUrl().toString()`.
    pub fn dav_url_string(&self) -> String {
        format!(
            "{}://{}{}",
            self.url.scheme,
            self.url.authority,
            utf8_percent_encode(&self.dav_url_path(), PATH_ENCODE_SET)
        )
    }

    /// `makeDavUrl(relativePath)` as a decoded path.
    pub fn dav_path_for(&self, relative_path: &str) -> String {
        concat_url_path(&self.dav_url_path(), relative_path)
    }

    /// `makeAccountUrl(relativePath)` as a decoded path.
    pub fn account_path_for(&self, relative_path: &str) -> String {
        concat_url_path(&self.url.path, relative_path)
    }

    /// The URI for a decoded absolute path.
    pub fn uri(&self, decoded_path: &str) -> Result<http::Uri, UrlError> {
        self.url.uri_for_path(decoded_path)
    }

    /// Builds a request with the headers every request carries
    /// (`AccessManager::createRequest` and `HttpCredentials`): user agent,
    /// `X-Request-ID`, authorization.
    pub fn build_request(
        &self,
        method: Method,
        uri: http::Uri,
        headers: HeaderMap,
        body: Body,
    ) -> (Request, String) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut req = http::Request::new(body);
        *req.method_mut() = method;
        *req.uri_mut() = uri;
        let h = req.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&self.user_agent) {
            h.insert(http::header::USER_AGENT, v);
        }
        if let Ok(v) = HeaderValue::from_str(&request_id) {
            h.insert("X-Request-ID", v);
        }
        if let Some(auth) = self.credentials().authorization_header() {
            h.insert(http::header::AUTHORIZATION, auth);
        }
        for (k, v) in headers.iter() {
            h.insert(k.clone(), v.clone());
        }
        (req, request_id)
    }

    /// Sends a request through the transport.
    pub async fn send(&self, request: Request) -> Result<Response, TransportError> {
        self.transport.send(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_server_url_parsing() {
        let u = ServerUrl::parse("http://admin:se%40cret@localhost:8080/owncloud").unwrap();
        assert_eq!(u.scheme, "http");
        assert_eq!(u.authority, "localhost:8080");
        assert_eq!(u.host, "localhost");
        assert_eq!(u.path, "/owncloud");
        assert_eq!(u.user, "admin");
        assert_eq!(u.password, "se@cret");
        assert_eq!(
            u.to_credential_free_string(),
            "http://localhost:8080/owncloud"
        );
        let u = ServerUrl::parse("cloud.example.com").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.path, "");
        assert!(ServerUrl::parse("http://").is_err());
    }

    #[test]
    fn derived_concat_and_encoding() {
        assert_eq!(
            concat_url_path("/owncloud", "remote.php"),
            "/owncloud/remote.php"
        );
        assert_eq!(
            concat_url_path("/owncloud/", "/remote.php"),
            "/owncloud/remote.php"
        );
        assert_eq!(concat_url_path("", "/x"), "/x");
        let u = ServerUrl::parse("https://h/").unwrap();
        assert_eq!(
            u.uri_for_path("/a b/100%#?é").unwrap().to_string(),
            "https://h/a%20b/100%25%23%3F%C3%A9"
        );
        assert_eq!(to_percent_encoding_keep_slash("/a b/(x)"), "/a%20b/%28x%29");
    }

    #[test]
    fn derived_account_paths_and_versions() {
        struct Never;
        impl Transport for Never {
            fn send(&self, _: Request) -> crate::BoxFuture<'_, Result<Response, TransportError>> {
                Box::pin(std::future::pending())
            }
        }
        let a = Account::new(
            ServerUrl::parse("http://admin:admin@localhost/owncloud").unwrap(),
            Arc::new(Never),
        );
        assert_eq!(a.dav_user(), "admin");
        assert_eq!(a.dav_path(), "/remote.php/dav/files/admin/");
        assert_eq!(a.dav_url_path(), "/owncloud/remote.php/dav/files/admin/");
        assert_eq!(
            a.dav_path_for("/A/a1"),
            "/owncloud/remote.php/dav/files/admin/A/a1"
        );
        assert_eq!(
            a.dav_url_string(),
            "http://localhost/owncloud/remote.php/dav/files/admin/"
        );
        a.set_server_version("28.0.3");
        assert!(a.server_has_mount_root_property());
        a.set_server_version("10.0.0");
        assert!(!a.server_has_mount_root_property());
        assert_eq!(a.server_version_int(), make_server_version(10, 0, 0));
        let (req, id) = a.build_request(
            Method::GET,
            a.uri("/x").unwrap(),
            HeaderMap::new(),
            Body::empty(),
        );
        assert_eq!(req.headers()["X-Request-ID"], id.as_str());
        assert_eq!(req.headers()["authorization"], "Basic YWRtaW46YWRtaW4=");
    }
}
