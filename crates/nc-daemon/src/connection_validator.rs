// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/connectionvalidator.{h,cpp}` (ConnectionValidator
// and TermsOfServiceChecker) and of the user fetch of `src/gui/userinfo.cpp`
// (nextcloud/desktop v34.0.5).

//! `ConnectionValidator`: checks that the server is reachable and that the
//! credentials work, and reports a [`Status`].
//!
//! Upstream chains network jobs through signals and slots and reports with
//! `connectionResult(status, errors)`; here the chain is one async function
//! returning the same status and error list.
//!
//! Not ported: the system proxy lookup (the transport uses the system proxy
//! settings itself), the macOS local network permission check, direct
//! editors, end-to-end encryption initialization, the public share link
//! accounts.

use std::time::Duration;

use nc_dav::reply::NetworkError;
use nc_dav::{Account, Capabilities, JobOptions, Reply};

const LOG: &str = "nextcloud.gui.connectionvalidator";

/// `ConnectionValidator::Status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Undefined,
    Connected,
    NotConfigured,
    /// The server version is too old
    ServerVersionMismatch,
    /// Credentials aren't ready
    CredentialsNotReady,
    /// AuthenticationRequiredError
    CredentialsWrong,
    /// SSL handshake error, certificate rejected by user?
    SslError,
    /// Error retrieving status.php
    StatusNotFound,
    /// 204 URL received one of redirect HTTP codes (301-307), possibly a captive portal
    StatusRedirect,
    /// 503 on authed request
    ServiceUnavailable,
    /// maintenance enabled in status.php
    MaintenanceMode,
    /// actually also used for other errors on the authed request
    Timeout,
    NeedToSignTermsOfService,
}

impl Status {
    /// The `Q_ENUM` key.
    pub fn name(self) -> &'static str {
        match self {
            Self::Undefined => "Undefined",
            Self::Connected => "Connected",
            Self::NotConfigured => "NotConfigured",
            Self::ServerVersionMismatch => "ServerVersionMismatch",
            Self::CredentialsNotReady => "CredentialsNotReady",
            Self::CredentialsWrong => "CredentialsWrong",
            Self::SslError => "SslError",
            Self::StatusNotFound => "StatusNotFound",
            Self::StatusRedirect => "StatusRedirect",
            Self::ServiceUnavailable => "ServiceUnavailable",
            Self::MaintenanceMode => "MaintenanceMode",
            Self::Timeout => "Timeout",
            Self::NeedToSignTermsOfService => "NeedToSignTermsOfService",
        }
    }
}

/// `DefaultCallingIntervalMsec`.
pub const DEFAULT_CALLING_INTERVAL: Duration = Duration::from_millis(62 * 1000);

/// `timeoutToUseMsec`: makes sure we get tried often enough without
/// "ConnectionValidator already running".
pub fn timeout_to_use() -> Duration {
    Duration::from_millis(1000u64.max(DEFAULT_CALLING_INTERVAL.as_millis() as u64 - 5 * 1000))
}

fn job_options() -> JobOptions {
    JobOptions {
        timeout: timeout_to_use(),
        ..JobOptions::default()
    }
}

/// `HttpCredentials::stillValid(reply)`.
fn credentials_still_valid(reply: &Reply) -> bool {
    reply.error != NetworkError::AuthenticationRequiredError
}

/// What the validator found out besides the status.
#[derive(Clone, Debug, Default)]
pub struct ValidationResult {
    pub status: Status,
    pub errors: Vec<String>,
}

/// The ConnectionValidator of one check.
pub struct ConnectionValidator<'a> {
    account: &'a Account,
    credentials_ready: bool,
    errors: Vec<String>,
}

impl<'a> ConnectionValidator<'a> {
    /// `credentials_ready`: `creds->ready()` (a password or app password is
    /// known).
    pub fn new(account: &'a Account, credentials_ready: bool) -> Self {
        Self {
            account,
            credentials_ready,
            errors: Vec::new(),
        }
    }

    fn report(self, status: Status, previous_errors: &[String]) -> ValidationResult {
        // TODO upstream: notify user of errors
        if !self.errors.is_empty() && previous_errors != self.errors.as_slice() {
            log::warn!(target: LOG, "Connection issues: {:?}", self.errors);
        }
        ValidationResult {
            status,
            errors: self.errors,
        }
    }

    /// `checkServerAndAuth()`: the redirect check, status.php, the
    /// authentication, the capabilities, the terms of service and the user.
    pub async fn check_server_and_auth(mut self, previous_errors: &[String]) -> ValidationResult {
        log::debug!(target: LOG, "Checking server and authentication");
        // slotCheckRedirectCostFreeUrl
        let status_code =
            nc_dav::jobs::check_redirect_cost_free_url(self.account, &job_options()).await;
        match status_code {
            Err(reply) if reply.timed_out => {
                // slotJobTimeout
                self.errors.push("Timeout".to_owned());
                return self.report(Status::Timeout, previous_errors);
            }
            Ok(code)
            | Err(Reply {
                http_status: code, ..
            }) if (301..=307).contains(&code) => {
                return self.report(Status::StatusRedirect, previous_errors);
            }
            _ => {}
        }
        // slotCheckServerAndAuth
        match nc_dav::jobs::check_server(self.account, &job_options()).await {
            Ok(info) => {
                // slotStatusFound
                let server_version = info
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                log::info!(target: LOG, "** Application: Nextcloud found: {} with version {} ({server_version})", self.account.url().to_credential_free_string(), info.get("versionstring").and_then(|v| v.as_str()).unwrap_or_default());
                if !server_version.is_empty() {
                    self.set_and_check_server_version(&server_version, false);
                }
                // Check for maintenance mode: Servers send "true", so go through QVariant
                // to parse it correctly.
                if variant_to_bool(info.get("maintenance")) {
                    return self.report(Status::MaintenanceMode, previous_errors);
                }
            }
            Err(reply) => {
                if reply.timed_out {
                    // slotJobTimeout
                    self.errors.push("Timeout".to_owned());
                    return self.report(Status::Timeout, previous_errors);
                }
                // slotNoStatusFound
                log::warn!(target: LOG, "{:?} {}", reply.error, reply.error_string());
                if reply.error == NetworkError::SslHandshakeFailedError {
                    return self.report(Status::SslError, previous_errors);
                }
                let error = if !credentials_still_valid(&reply) {
                    // Note: Why would this happen on a status.php request?
                    "Authentication error: Either username or password are wrong.".to_owned()
                } else {
                    reply.error_string()
                };
                self.errors.push(error);
                return self.report(Status::StatusNotFound, previous_errors);
            }
        }
        // now check the authentication
        self.check_authentication_inner(true, previous_errors).await
    }

    /// `checkAuthentication()`: an authenticated PROPFIND as a minimal ping
    /// (used when already connected).
    pub async fn check_authentication(self, previous_errors: &[String]) -> ValidationResult {
        self.check_authentication_inner(false, previous_errors)
            .await
    }

    async fn check_authentication_inner(
        mut self,
        is_checking_server_and_auth: bool,
        previous_errors: &[String],
    ) -> ValidationResult {
        if !self.credentials_ready {
            return self.report(Status::CredentialsNotReady, previous_errors);
        }
        // simply GET the webdav root, will fail if credentials are wrong.
        // continue in slotAuthCheck here :-)
        log::debug!(target: LOG, "# Check whether authenticated propfind works.");
        match nc_dav::jobs::propfind(self.account, "/", &["getlastmodified"], &job_options()).await
        {
            Ok(_) => {}
            Err(reply) => {
                // slotAuthFailed
                let mut stat = Status::Timeout;
                if reply.error == NetworkError::SslHandshakeFailedError {
                    self.errors.push(reply.error_string_parsing_body());
                    stat = Status::SslError;
                } else if reply.error == NetworkError::AuthenticationRequiredError
                    || !credentials_still_valid(&reply)
                {
                    log::warn!(target: LOG, "******** Password is wrong! {:?} {}", reply.error, reply.error_string());
                    self.errors
                        .push("The provided credentials are not correct".to_owned());
                    stat = Status::CredentialsWrong;
                } else if reply.error != NetworkError::NoError {
                    self.errors.push(reply.error_string_parsing_body());
                    if reply.http_status == 503 {
                        self.errors.clear();
                        stat = Status::ServiceUnavailable;
                    } else if reply.http_status == 403 {
                        let dav_exception = reply.error_string_parsing_body_exception();
                        if dav_exception == r"OCA\TermsOfService\TermsNotSignedException" {
                            log::info!(target: LOG, "The terms of service need to be signed");
                            stat = Status::NeedToSignTermsOfService;
                        }
                    }
                }
                return self.report(stat, previous_errors);
            }
        }
        // slotAuthSuccess
        self.errors.clear();
        if !is_checking_server_and_auth {
            return self.report(Status::Connected, previous_errors);
        }
        // checkServerCapabilities
        let (json, _, _) = nc_dav::jobs::json_api(
            self.account,
            "ocs/v1.php/cloud/capabilities",
            &job_options(),
        )
        .await;
        // slotCapabilitiesRecieved (also emitted with an empty document on
        // a network error, which leaves the capabilities empty)
        let json = json.unwrap_or(serde_json::Value::Null);
        let mut caps = json["ocs"]["data"]["capabilities"].clone();
        if !caps.is_object() {
            caps = serde_json::json!({});
        }
        log::info!(target: LOG, "Server capabilities {caps}");
        let server_version = caps["core"]["status"]["version"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.account.set_capabilities(Capabilities::from_json(caps));
        // New servers also report the version in the capabilities
        if !server_version.is_empty() {
            self.set_and_check_server_version(&server_version, true);
        }
        // checkServerTermsOfService
        if TermsOfServiceChecker::check(self.account).await {
            return self.report(Status::NeedToSignTermsOfService, previous_errors);
        }
        // fetchUser
        fetch_user(self.account).await;
        self.report(Status::Connected, previous_errors)
    }

    /// `setAndCheckServerVersion(version)`: records the version (the
    /// version check itself is disabled upstream).
    fn set_and_check_server_version(&self, version: &str, _from_capabilities: bool) {
        log::info!(target: LOG, "{} has server version {version}", self.account.url().to_credential_free_string());
        self.account.set_server_version(version);
    }
}

/// `QVariant::toBool()` of a JSON value: `true`, a non-zero number, or a
/// string other than "", "0" and "false".
fn variant_to_bool(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(serde_json::Value::String(s)) => {
            let s = s.to_lowercase();
            !(s.is_empty() || s == "0" || s == "false")
        }
        _ => false,
    }
}

/// `TermsOfServiceChecker`.
pub struct TermsOfServiceChecker;

impl TermsOfServiceChecker {
    /// `checkServerTermsOfService()` until `done()`: whether the user
    /// needs to sign the terms of service. A network error leaves the
    /// answer at "no need to sign".
    pub async fn check(account: &Account) -> bool {
        let (json, _, reply) = nc_dav::jobs::json_api(
            account,
            "ocs/v2.php/apps/terms_of_service/terms",
            &job_options(),
        )
        .await;
        if !reply.is_ok() {
            log::info!(target: LOG, "network error {:?}", reply.error);
        }
        // slotServerTermsOfServiceRecieved
        let need_to_sign = match json {
            Some(j) if j.get("ocs").is_some() => {
                !j["ocs"]["data"]["hasSigned"].as_bool().unwrap_or(false)
            }
            _ => false,
        };
        log::info!(target: LOG, "_needToSign {}", if need_to_sign { "need to sign" } else { "no need to sign" });
        need_to_sign
    }
}

/// `UserInfo::slotFetchInfo` / `slotUpdateLastInfo` (the parts the sync
/// needs): the user id and display name.
pub async fn fetch_user(account: &Account) {
    let opts = JobOptions {
        timeout: Duration::from_secs(20),
        ..JobOptions::default()
    };
    let (json, _, reply) = nc_dav::jobs::json_api(account, "ocs/v1.php/cloud/user", &opts).await;
    let Some(json) = json else {
        log::warn!(target: LOG, "Could not fetch the user info: {}", reply.error_string());
        return;
    };
    let data = &json["ocs"]["data"];
    if let Some(id) = data["id"].as_str()
        && !id.is_empty()
    {
        let dav_user = account.dav_user();
        if !dav_user.is_empty() && !dav_user.eq_ignore_ascii_case(id) {
            // TODO upstream: the error message should be in the UI
            log::info!(target: LOG, "Authenticated with the wrong user! Please login with the account: {dav_user}");
        } else {
            account.set_dav_user(id);
        }
    }
    if let Some(name) = data["display-name"].as_str() {
        account.set_dav_display_name(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_variant_to_bool() {
        assert!(variant_to_bool(Some(&serde_json::json!(true))));
        assert!(variant_to_bool(Some(&serde_json::json!("true"))));
        assert!(!variant_to_bool(Some(&serde_json::json!(false))));
        assert!(!variant_to_bool(Some(&serde_json::json!("false"))));
        assert!(!variant_to_bool(None));
    }

    #[test]
    fn derived_timeout_to_use_is_57_seconds() {
        assert_eq!(timeout_to_use(), Duration::from_secs(57));
    }
}
