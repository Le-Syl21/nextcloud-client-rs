/*
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

// Port of upstream test/testcapabilities.cpp (nextcloud/desktop v34.0.5).
//
// `QVariantMap` capabilities are built as JSON objects. `QMimeDatabase`
// lookups are replaced by the types it returns from shared-mime-info
// (`mime_type_for_file`).

use nc_dav::Capabilities;
use nc_dav::capabilities::{MimeType, PushNotificationTypes};
use serde_json::{Value, json};

const CLIENT_INTEGRATION: &str = r#"
{
    "client_integration": {
        "analytics": {
            "version": 0.1,
            "context-menu": [
                {
                    "name": "Visualize data in Analytics",
                    "url": "/ocs/v2.php/apps/analytics/createFromDataFile",
                    "method": "POST",
                    "mimetype_filters": "text/csv",
                    "params": {
                        "fileId": "{fileId}"
                    },
                    "icon": "/apps/analytics/img/app.svg"
                },
                {
                    "name": "Visualize data in Analytics",
                    "url": "/ocs/v2.php/apps/analytics/createFromDataFile",
                    "method": "POST",
                    "mimetype_filters": "",
                    "params": {
                        "fileId": "{fileId}"
                    },
                    "icon": "/apps/analytics/img/app.svg"
                }
            ]
        },
        "assistant": {
            "version": 0.1,
            "context-menu": [
                {
                    "name": "Summarize using AI",
                    "url": "/ocs/v2.php/apps/assistant/api/v1/file-action/{fileId}/core:text2text:summary",
                    "method": "POST",
                    "mimetype_filters": "text/, application/msword, application/vnd.openxmlformats-officedocument.wordprocessingml.document, application/vnd.oasis.opendocument.text, application/pdf",
                    "icon": "/apps/assistant/img/client_integration/summarize.svg"
                },
                {
                    "name": "Transcribe audio using AI",
                    "url": "/ocs/v2.php/apps/assistant/api/v1/file-action/{fileId}/core:audio2text",
                    "method": "POST",
                    "mimetype_filters": "audio/",
                    "icon": "/apps/assistant/img/client_integration/speech_to_text.svg"
                },
                {
                    "name": "Text-To-Speech using AI",
                    "url": "/ocs/v2.php/apps/assistant/api/v1/file-action/{fileId}/core:text2speech",
                    "method": "POST",
                    "mimetype_filters": "text/, application/msword, application/vnd.openxmlformats-officedocument.wordprocessingml.document, application/vnd.oasis.opendocument.text, application/pdf",
                    "icon": "/apps/assistant/img/client_integration/text_to_speech.svg"
                }
            ]
        },
        "contacts": {
            "version": 0.1,
            "context-menu": [
                {
                    "name": "Import contacts",
                    "url": "/ocs/v2.php/apps/contacts/api/v1/import/{fileId}",
                    "method": "POST",
                    "mimetype_filters": "text/vcard"
                }
            ]
        }
    }
}
"#;

fn caps(v: Value) -> Capabilities {
    Capabilities::from_json(v)
}

/// `QMimeDatabase::mimeTypeForFile(name, QMimeDatabase::MatchExtension)`
/// for the files of `testFileActionsByMimeType_returnContextMenu`, as
/// shared-mime-info defines them (globs2 and subclasses, plus Qt's implicit
/// `text/plain` parent of `text/*` and `application/octet-stream` parent of
/// everything).
fn mime_type_for_file(file_name: &str) -> MimeType {
    let (name, ancestors): (&str, &[&str]) = match file_name.rsplit('.').next() {
        Some("mp4") => ("video/mp4", &["application/octet-stream"]),
        Some("csv") => ("text/csv", &["text/plain", "application/octet-stream"]),
        Some("vcf") => ("text/vcard", &["text/plain", "application/octet-stream"]),
        Some("odt") => (
            "application/vnd.oasis.opendocument.text",
            &["application/zip", "application/octet-stream"],
        ),
        _ => ("application/octet-stream", &[]),
    };
    MimeType {
        name: name.to_owned(),
        ancestors: ancestors.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn test_push_notifications_available_push_notifications_for_activities_available_return_true() {
    let type_list = vec!["activities"];

    let capabilities = caps(json!({ "notify_push": { "type": type_list } }));
    let activities_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::ACTIVITIES);

    assert!(activities_push_notifications_available);
}

#[test]
fn test_push_notifications_available_push_notifications_for_activities_not_available_return_false()
{
    let type_list = vec!["noactivities"];

    let capabilities = caps(json!({ "notify_push": { "type": type_list } }));
    let activities_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::ACTIVITIES);

    assert!(!activities_push_notifications_available);
}

#[test]
fn test_push_notifications_available_push_notifications_for_files_available_return_true() {
    let type_list = vec!["files"];

    let capabilities = caps(json!({ "notify_push": { "type": type_list } }));
    let files_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::FILES);

    assert!(files_push_notifications_available);
}

#[test]
fn test_push_notifications_available_push_notifications_for_files_not_available_return_false() {
    let type_list = vec!["nofiles"];

    let capabilities = caps(json!({ "notify_push": { "type": type_list } }));
    let files_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::FILES);

    assert!(!files_push_notifications_available);
}

#[test]
fn test_push_notifications_available_push_notifications_for_notifications_available_return_true() {
    let type_list = vec!["notifications"];

    let capabilities = caps(json!({ "notify_push": { "type": type_list } }));
    let notifications_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::NOTIFICATIONS);

    assert!(notifications_push_notifications_available);
}

#[test]
fn test_push_notifications_available_push_notifications_for_notifications_not_available_return_false()
 {
    let type_list = vec!["nonotifications"];

    let capabilities = caps(json!({ "notify_push": { "type": type_list } }));
    let notifications_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::NOTIFICATIONS);

    assert!(!notifications_push_notifications_available);
}

#[test]
fn test_push_notifications_available_push_notifications_not_available_return_false() {
    let capabilities = caps(json!({}));
    let activities_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::ACTIVITIES);
    let files_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::FILES);
    let notifications_push_notifications_available = capabilities
        .available_push_notifications()
        .test_flag(PushNotificationTypes::NOTIFICATIONS);

    assert!(!activities_push_notifications_available);
    assert!(!files_push_notifications_available);
    assert!(!notifications_push_notifications_available);
}

#[test]
fn test_push_notifications_web_socket_url_url_available_return_url() {
    let websocket_url = "testurl";

    let capabilities =
        caps(json!({ "notify_push": { "endpoints": { "websocket": websocket_url } } }));

    assert_eq!(
        capabilities.push_notifications_web_socket_url(),
        websocket_url
    );
}

#[test]
fn test_user_status_user_status_available_return_true() {
    let capabilities = caps(json!({ "user_status": { "enabled": true } }));

    assert!(capabilities.user_status());
}

#[test]
fn test_user_status_user_status_not_available_return_false() {
    let capabilities = caps(json!({ "user_status": { "enabled": false } }));

    assert!(!capabilities.user_status());
}

#[test]
fn test_user_status_user_status_not_in_capabilites_return_false() {
    let capabilities = caps(json!({}));

    assert!(!capabilities.user_status());
}

#[test]
fn test_user_status_supports_emoji_supports_emoji_available_return_true() {
    let capabilities = caps(json!({ "user_status": { "enabled": true, "supports_emoji": true } }));

    // Upstream checks userStatus() here, not userStatusSupportsEmoji().
    assert!(capabilities.user_status());
}

#[test]
fn test_user_status_supports_emoji_supports_emoji_not_available_return_false() {
    let capabilities = caps(json!({ "user_status": { "enabled": true, "supports_emoji": false } }));

    assert!(!capabilities.user_status_supports_emoji());
}

#[test]
fn test_user_status_supports_emoji_supports_emoji_not_in_capabilites_return_false() {
    let capabilities = caps(json!({ "user_status": { "enabled": true } }));

    assert!(!capabilities.user_status_supports_emoji());
}

#[test]
fn test_user_status_supports_busy_supports_busy_available_return_true() {
    let capabilities = caps(json!({ "user_status": { "enabled": true, "supports_busy": true } }));

    assert!(capabilities.user_status_supports_busy());
}

#[test]
fn test_user_status_supports_busy_supports_busy_not_available_return_false() {
    let capabilities = caps(json!({ "user_status": { "enabled": true, "supports_busy": false } }));

    assert!(!capabilities.user_status_supports_busy());
}

#[test]
fn test_user_status_supports_busy_supports_busy_not_in_capabilities_return_false() {
    let capabilities = caps(json!({ "user_status": { "enabled": true } }));

    assert!(!capabilities.user_status_supports_busy());
}

#[test]
fn test_user_status_supports_busy_user_status_not_enabled_return_false() {
    let capabilities = caps(json!({ "user_status": { "enabled": false, "supports_busy": true } }));

    assert!(!capabilities.user_status_supports_busy());
}

#[test]
fn test_share_default_permissions_default_share_permissions_not_in_capabilities_return_zero() {
    let capabilities = caps(json!({ "files_sharing": { "api_enabled": false } }));
    let default_share_permissions_not_in_capabilities = capabilities.share_default_permissions();

    assert_eq!(default_share_permissions_not_in_capabilities, 0);
}

#[test]
fn test_share_default_permissions_default_share_permissions_available_return_permissions() {
    let capabilities =
        caps(json!({ "files_sharing": { "api_enabled": true, "default_permissions": 31 } }));
    let default_share_permissions_available = capabilities.share_default_permissions();

    assert_eq!(default_share_permissions_available, 31);
}

#[test]
fn test_bulk_upload_available_bulk_upload_available_return_true() {
    let capabilities = caps(json!({ "dav": { "bulkupload": "1.0" } }));
    let bulkupload_available = capabilities.bulk_upload();

    assert!(bulkupload_available);
}

#[test]
fn test_files_lock_available_files_lock_available_return_true() {
    let capabilities = caps(json!({ "files": { "locking": "1.0" } }));
    let files_lock_available = capabilities.files_lock_available();

    assert!(files_lock_available);
}

#[test]
fn test_support_has_valid_subscription_return_true() {
    let capabilities = caps(json!({ "support": { "hasValidSubscription": "true" } }));
    let server_has_valid_subscription = capabilities.server_has_valid_subscription();

    assert!(server_has_valid_subscription);
}

#[test]
fn test_support_desktop_enterprise_channel_return_string() {
    let default_channel = "stable";

    let capabilities = caps(json!({ "support": { "desktopEnterpriseChannel": default_channel } }));
    let enterprise_channel = capabilities.desktop_enterprise_channel();

    assert_eq!(enterprise_channel.as_deref(), Some(default_channel));
}

#[test]
fn test_server_has_client_integration_return_true() {
    let capabilities = caps(serde_json::from_str(CLIENT_INTEGRATION).unwrap());
    let has_client_integration = capabilities.server_has_client_integration();
    assert!(has_client_integration);
}

#[test]
fn test_file_actions_by_mime_type_return_context_menu() {
    let capabilities = caps(serde_json::from_str(CLIENT_INTEGRATION).unwrap());
    let mut context_menu = capabilities.file_actions_by_mime_type(&mime_type_for_file("audio.mp4"));
    assert_eq!(context_menu.len(), 4);
    context_menu = capabilities.file_actions_by_mime_type(&mime_type_for_file("spreadsheet.csv"));
    assert_eq!(context_menu.len(), 5);
    context_menu = capabilities.file_actions_by_mime_type(&mime_type_for_file("contact.vcf"));
    assert_eq!(context_menu.len(), 5);
    context_menu = capabilities.file_actions_by_mime_type(&mime_type_for_file("document.odt"));
    assert_eq!(context_menu.len(), 6);
}
