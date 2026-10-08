// SPDX-FileCopyrightText: 2019 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/remotewipe.cpp` (nextcloud/desktop v34.0.5).

//! `RemoteWipe`: the server-side "wipe device" of an app password.
//!
//! When a request of the account fails because of the credentials
//! (`Account::handleInvalidCredentials`), the app password is sent to
//! `index.php/core/wipe/check`. When the server answers `{"wipe": true}`,
//! the folder manager deletes every folder of the account (journal and
//! local files, `FolderMan::slotWipeFolderForAccount`), the account is
//! deleted with its credentials, and `index.php/core/wipe/success` is told.
//! Otherwise the account asks for new credentials.
//!
//! The requests are sent like upstream's own `QNetworkAccessManager`: no
//! credentials, the token in an `application/x-www-form-urlencoded` body.
//! The wiring (who calls what when) is in `folder_man.rs`.

use http::{HeaderMap, HeaderValue, Method};
use nc_dav::{Account, Body, JobOptions, Reply, Target};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

const LOG: &str = "nextcloud.gui.remotewipe";

/// What `QUrlQuery::query(QUrl::FullyEncoded)` leaves unencoded in a value.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The remote wipe state of one account.
#[derive(Clone, Debug, Default)]
pub struct RemoteWipe {
    /// The app password of the last check (`_appPassword`).
    pub app_password: String,
    /// What `Account::retrieveAppPassword()` reads from the keychain: the
    /// app password the account was loaded with.
    pub stored_app_password: String,
    /// `Account::isRemoteWipeRequested_HACK()`.
    pub remote_wipe_requested: bool,
}

/// POSTs `token=<app password>` to `path` (relative to the account URL).
async fn post_token(account: &Account, path: &str, app_password: &str) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let body = format!("token={}", utf8_percent_encode(app_password, QUERY_VALUE));
    let opts = JobOptions {
        // A QNetworkAccessManager of its own: no Authorization header and
        // no AbstractNetworkJob credential handling.
        dont_add_credentials: true,
        ignore_credential_failure: true,
        ..JobOptions::default()
    };
    nc_dav::jobs::send(
        account,
        Method::POST,
        &Target::Account(path.to_owned()),
        headers,
        Body::from(body),
        &opts,
    )
    .await
}

/// Logs the failure of a reply the way `slotCheckJob` and
/// `slotNotifyServerSuccessFinished` do. Returns the parsed JSON object
/// when the reply is a success with a JSON object body.
fn parse_reply(reply: &Reply, endpoint: &str, trailing_dot: &str) -> Option<serde_json::Value> {
    let parsed = serde_json::from_slice::<serde_json::Value>(&reply.body);
    // QJsonDocument::object() of a non-object is an empty object.
    let json = match &parsed {
        Ok(v) if v.is_object() => v.clone(),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };
    if reply.is_ok() && parsed.is_ok() {
        return Some(json);
    }
    let error_from_json = json["error"].as_str().unwrap_or_default();
    if !error_from_json.is_empty() {
        log::warn!(target: LOG, "Error returned from the server: <em>{}</em>", error_from_json);
    } else if !reply.is_ok() {
        log::warn!(target: LOG, "There was an error accessing the '{endpoint}' endpoint: <br><em>{}</em>", reply.error_string());
    } else if let Err(e) = &parsed {
        log::warn!(target: LOG, "Could not parse the JSON returned from the server: <br><em>{e}</em>");
    } else {
        log::warn!(target: LOG, "The reply from the server did not contain all expected fields{trailing_dot}");
    }
    None
}

/// `startCheckJobWithAppPassword(pwd)` and `slotCheckJob()`: whether the
/// server asks for a wipe. `None` when the app password is empty (no
/// check is made).
pub async fn check(account: &Account, app_password: &str) -> Option<bool> {
    if app_password.is_empty() {
        log::debug!(target: LOG, "not checking remote wipe status: app password is empty");
        return None;
    }
    let reply = post_token(account, "index.php/core/wipe/check", app_password).await;
    let mut wipe = false;
    // check for errors
    if let Some(json) = parse_reply(&reply, "token", "") {
        // check for wipe request
        if let Some(v) = json.get("wipe") {
            // QJsonValue::toBool(): true only for the JSON value true.
            wipe = v.as_bool() == Some(true);
        }
    }
    Some(wipe)
}

/// `notifyServerSuccess()` and `slotNotifyServerSuccessFinished()`.
pub async fn notify_server_success(account: &Account, app_password: &str, display_name: &str) {
    log::info!(target: LOG, "Notifying server about successful remote wipe for {display_name}");
    let reply = post_token(account, "index.php/core/wipe/success", app_password).await;
    let _ = parse_reply(&reply, "success", ".");
}

/// `Account::deleteAppToken()`: `DELETE ocs/v2.php/core/apppassword`.
pub async fn delete_app_token(account: &Account, display_name: &str) {
    let mut headers = HeaderMap::new();
    headers.insert("OCS-APIREQUEST", HeaderValue::from_static("true"));
    let reply = nc_dav::jobs::send(
        account,
        Method::DELETE,
        &Target::Account("ocs/v2.php/core/apppassword".to_owned()),
        headers,
        Body::empty(),
        &JobOptions::default(),
    )
    .await;
    if reply.http_status != 200 {
        log::warn!(target: "nextcloud.sync.account", "AppToken remove failed for user:  {display_name}  with code:  {}", reply.http_status);
    } else {
        log::info!(target: "nextcloud.sync.account", "AppToken for user:  {display_name}  has been removed.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_token_encoding() {
        assert_eq!(
            utf8_percent_encode("aB3-x_y.z~", QUERY_VALUE).to_string(),
            "aB3-x_y.z~"
        );
        assert_eq!(
            utf8_percent_encode("a b&c=d+", QUERY_VALUE).to_string(),
            "a%20b%26c%3Dd%2B"
        );
    }
}
