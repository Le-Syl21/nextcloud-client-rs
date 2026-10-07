/*
 * SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

// Port of upstream test/testpushnotifications.cpp (nextcloud/desktop v34.0.5).
//
// The tests run on a current-thread runtime, like the Qt event loop: the
// push notifications task only runs while the test awaits (a spy's `wait()`
// or the fake server's `wait_for_text_messages()`).

mod pushnotificationstestutils;

use std::sync::Arc;
use std::time::Duration;

use nc_dav::account::AccountPushEvent;
use nc_dav::{Account, HttpClientOptions, PushNotificationsEvent};
use pushnotificationstestutils::{FakeWebSocketServer, SignalSpy};

macro_rules! return_false_on_fail {
    ($e:expr) => {
        if !($e) {
            return false;
        }
    };
}

fn files_changed(e: &PushNotificationsEvent) -> bool {
    matches!(e, PushNotificationsEvent::FilesChanged { .. })
}

fn file_ids_changed(e: &PushNotificationsEvent) -> bool {
    matches!(e, PushNotificationsEvent::FileIdsChanged { .. })
}

fn notifications_changed(e: &PushNotificationsEvent) -> bool {
    matches!(e, PushNotificationsEvent::NotificationsChanged { .. })
}

fn activities_changed(e: &PushNotificationsEvent) -> bool {
    matches!(e, PushNotificationsEvent::ActivitiesChanged { .. })
}

fn authentication_failed(e: &PushNotificationsEvent) -> bool {
    matches!(e, PushNotificationsEvent::AuthenticationFailed)
}

fn connection_lost(e: &PushNotificationsEvent) -> bool {
    matches!(e, PushNotificationsEvent::ConnectionLost)
}

fn push_notifications_disabled(e: &AccountPushEvent) -> bool {
    matches!(e, AccountPushEvent::PushNotificationsDisabled { .. })
}

fn spy(
    account: &Arc<Account>,
    filter: fn(&PushNotificationsEvent) -> bool,
) -> SignalSpy<PushNotificationsEvent> {
    SignalSpy::new(account.push_notifications().unwrap().subscribe(), filter)
}

fn account_spy(account: &Arc<Account>) -> SignalSpy<AccountPushEvent> {
    SignalSpy::new(
        account.subscribe_push_notifications_events(),
        push_notifications_disabled,
    )
}

fn verify_called_once_with_account(
    spy: &mut SignalSpy<PushNotificationsEvent>,
    account: &Arc<Account>,
) -> bool {
    return_false_on_fail!(spy.count() == 1);
    let event = spy.at(0);
    let account_from_spy = event.account().and_then(|a| a.upgrade());
    return_false_on_fail!(account_from_spy.is_some_and(|a| Arc::ptr_eq(&a, account)));

    true
}

async fn fail_three_authentication_attempts(
    fake_server: &mut FakeWebSocketServer,
    account: &Arc<Account>,
) -> bool {
    return_false_on_fail!(account.push_notifications().is_some());

    account
        .push_notifications()
        .unwrap()
        .set_reconnect_timer_interval(Duration::ZERO);

    let mut authentication_failed_spy = spy(account, authentication_failed);

    // Let three authentication attempts fail
    for _ in 0..3 {
        return_false_on_fail!(fake_server.wait_for_text_messages().await);
        return_false_on_fail!(fake_server.text_messages_count() == 2);
        let socket = fake_server.socket_for_text_message(0);
        fake_server.clear_text_messages();
        socket.send_text_message("err: Invalid credentials");
    }

    // Now the authenticationFailed Signal should be emitted
    return_false_on_fail!(authentication_failed_spy.wait().await);
    return_false_on_fail!(authentication_failed_spy.count() == 1);

    true
}

// initTestCase: n/a (logger settings and QStandardPaths test mode).

#[tokio::test]
async fn test_try_reconnect_capabilites_report_push_notifications_available_reconnect_for_ever() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();

    // Let if fail a few times
    assert!(fail_three_authentication_attempts(&mut fake_server, &account).await);
    account.push_notifications().unwrap().setup();
    assert!(fail_three_authentication_attempts(&mut fake_server, &account).await);

    account.set_push_notifications_reconnect_interval(Duration::ZERO);

    // Push notifications should try to reconnect
    assert!(fake_server.authenticate_account(&account).await.is_some());

    account.set_push_notifications_reconnect_interval(Duration::from_secs(60 * 2));
}

#[tokio::test]
async fn test_setup_correct_credentials_authenticate_and_emit_ready() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();

    assert!(
        fake_server
            .authenticate_account_with(
                &account,
                |push_notifications| {
                    (
                        SignalSpy::new(push_notifications.subscribe(), files_changed),
                        SignalSpy::new(push_notifications.subscribe(), notifications_changed),
                        SignalSpy::new(push_notifications.subscribe(), activities_changed),
                    )
                },
                |(
                    mut files_changed_spy,
                    mut notifications_changed_spy,
                    mut activities_changed_spy,
                )| {
                    assert!(verify_called_once_with_account(
                        &mut files_changed_spy,
                        &account
                    ));
                    assert!(verify_called_once_with_account(
                        &mut notifications_changed_spy,
                        &account
                    ));
                    assert!(verify_called_once_with_account(
                        &mut activities_changed_spy,
                        &account
                    ));
                },
            )
            .await
            .is_some()
    );
}

#[tokio::test]
async fn test_setup_https_account_with_plaintext_web_socket_does_not_send_credentials() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let web_socket_url = fake_server.url().to_owned();
    let account = FakeWebSocketServer::create_account_with(
        "user",
        "app-password",
        "https://cloud.example.test",
        &web_socket_url,
        HttpClientOptions::default(),
    );

    assert!(account.push_notifications().is_none());
    assert_eq!(fake_server.text_messages_count(), 0);

    account.try_setup_push_notifications();

    // Give a wrongly started websocket the time to connect and authenticate
    // (upstream checks right away, its websocket being asynchronous too).
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(account.push_notifications().is_none());
    assert_eq!(fake_server.text_messages_count(), 0);
}

#[tokio::test]
async fn test_on_web_socket_text_message_received_notify_file_message_emit_files_changed() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let socket = fake_server.authenticate_account(&account).await;
    let socket = socket.expect("authenticated");
    let mut files_changed_spy = spy(&account, files_changed);

    socket.send_text_message("notify_file");

    // filesChanged signal should be emitted
    assert!(files_changed_spy.wait().await);
    assert!(verify_called_once_with_account(
        &mut files_changed_spy,
        &account
    ));
}

#[tokio::test]
async fn test_on_web_socket_text_message_received_notify_file_id_message_emit_files_changed() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let socket = fake_server.authenticate_account(&account).await;
    let socket = socket.expect("authenticated");
    let mut files_changed_spy = spy(&account, file_ids_changed);

    socket.send_text_message("notify_file_id [1,2,3,5,8,13,21,34,55,89,144]");

    // filesChanged signal should be emitted
    assert!(files_changed_spy.wait().await);
    assert!(verify_called_once_with_account(
        &mut files_changed_spy,
        &account
    ));
    let expected: Vec<i64> = vec![1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144];
    let PushNotificationsEvent::FileIdsChanged { file_ids, .. } = files_changed_spy.at(0) else {
        panic!("not fileIdsChanged");
    };
    assert_eq!(file_ids, expected);
}

#[tokio::test]
async fn test_on_web_socket_text_message_received_notify_activity_message_emit_notification() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let socket = fake_server.authenticate_account(&account).await;
    let socket = socket.expect("authenticated");
    let mut activity_spy = spy(&account, activities_changed);
    assert!(activity_spy.is_valid());

    // Send notify_file push notification
    socket.send_text_message("notify_activity");

    // notification signal should be emitted
    assert!(activity_spy.wait().await);
    assert!(verify_called_once_with_account(&mut activity_spy, &account));
}

#[tokio::test]
async fn test_on_web_socket_text_message_received_notify_notification_message_emit_notification() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let socket = fake_server.authenticate_account(&account).await;
    let socket = socket.expect("authenticated");
    let mut notification_spy = spy(&account, notifications_changed);
    assert!(notification_spy.is_valid());

    // Send notify_file push notification
    socket.send_text_message("notify_notification");

    // notification signal should be emitted
    assert!(notification_spy.wait().await);
    assert!(verify_called_once_with_account(
        &mut notification_spy,
        &account
    ));
}

#[tokio::test]
async fn test_on_web_socket_text_message_received_invalid_credentials_message_reconnect_web_socket()
{
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    // Need to set reconnect timer interval to zero for tests
    account
        .push_notifications()
        .unwrap()
        .set_reconnect_timer_interval(Duration::ZERO);

    // Wait for authentication attempt and then sent invalid credentials
    assert!(fake_server.wait_for_text_messages().await);
    assert_eq!(fake_server.text_messages_count(), 2);
    let socket = fake_server.socket_for_text_message(0);
    let first_password_sent = fake_server.text_message(1);
    assert_eq!(first_password_sent, account.credentials().password);
    fake_server.clear_text_messages();
    socket.send_text_message("err: Invalid credentials");

    // Wait for a new authentication attempt
    assert!(fake_server.wait_for_text_messages().await);
    assert_eq!(fake_server.text_messages_count(), 2);
    let second_password_sent = fake_server.text_message(1);
    assert_eq!(second_password_sent, account.credentials().password);
}

#[tokio::test]
async fn test_on_web_socket_error_connection_lost_emit_connection_lost() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let mut connection_lost_spy = spy(&account, connection_lost);
    let mut push_notifications_disabled_spy = account_spy(&account);
    assert!(connection_lost_spy.is_valid());

    // Wait for authentication and then sent a network error
    assert!(fake_server.wait_for_text_messages().await);
    assert_eq!(fake_server.text_messages_count(), 2);
    let socket = fake_server.socket_for_text_message(0);
    socket.abort();

    assert!(connection_lost_spy.wait().await);
    // Account handled connectionLost signal and disabled push notifications
    assert_eq!(push_notifications_disabled_spy.count(), 1);
}

#[tokio::test]
async fn test_setup_max_connection_attempts_reached_disable_push_notifications() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let mut push_notifications_disabled_spy = account_spy(&account);

    assert!(fail_three_authentication_attempts(&mut fake_server, &account).await);
    // Account disabled the push notifications
    assert_eq!(push_notifications_disabled_spy.count(), 1);
}

/// Adapted: upstream emits `sslErrors` on the client's `QWebSocket` by hand
/// after the authentication messages arrived. Here a real `wss` server with
/// a self-signed certificate makes the certificate check fail, so no
/// authentication message is sent.
#[tokio::test]
async fn test_on_web_socket_ssl_error_ssl_error_disable_push_notifications() {
    let fake_server = FakeWebSocketServer::new_secure().await;
    let account = fake_server.create_account();
    let mut authentication_failed_spy = spy(&account, authentication_failed);
    let mut push_notifications_disabled_spy = account_spy(&account);

    assert!(authentication_failed_spy.wait().await);

    // Account handled connectionLost signal and the authenticationFailed Signal should be emitted
    assert_eq!(push_notifications_disabled_spy.count(), 1);
}

#[tokio::test]
async fn test_account_web_socket_connection_lost_emit_notifications_disabled() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    // Need to set reconnect timer interval to zero for tests
    account
        .push_notifications()
        .unwrap()
        .set_reconnect_timer_interval(Duration::ZERO);
    let socket = fake_server.authenticate_account(&account).await;
    let socket = socket.expect("authenticated");

    let mut connection_lost_spy = spy(&account, connection_lost);
    assert!(connection_lost_spy.is_valid());

    let mut push_notifications_disabled_spy = account_spy(&account);
    assert!(push_notifications_disabled_spy.is_valid());

    // Wait for authentication and then sent a network error
    socket.abort();

    assert!(push_notifications_disabled_spy.wait().await);
    assert_eq!(push_notifications_disabled_spy.count(), 1);

    assert_eq!(connection_lost_spy.count(), 1);

    let AccountPushEvent::PushNotificationsDisabled {
        account: account_sent,
    } = push_notifications_disabled_spy.at(0)
    else {
        panic!("not pushNotificationsDisabled");
    };
    assert!(Arc::ptr_eq(&account_sent.upgrade().unwrap(), &account));
}

#[tokio::test]
async fn test_account_web_socket_authentication_failed_emit_notifications_disabled() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let mut push_notifications_disabled_spy = account_spy(&account);
    assert!(push_notifications_disabled_spy.is_valid());

    assert!(fail_three_authentication_attempts(&mut fake_server, &account).await);

    // Now the pushNotificationsDisabled Signal should be emitted
    assert_eq!(push_notifications_disabled_spy.count(), 1);
    let AccountPushEvent::PushNotificationsDisabled {
        account: account_sent,
    } = push_notifications_disabled_spy.at(0)
    else {
        panic!("not pushNotificationsDisabled");
    };
    assert!(Arc::ptr_eq(&account_sent.upgrade().unwrap(), &account));
}

#[tokio::test]
async fn test_ping_timeout_ping_timed_out_reconnect() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    assert!(fake_server.authenticate_account(&account).await.is_some());

    // Set the ping timeout interval to zero and check if the server attempts to authenticate again
    fake_server.clear_text_messages();
    account
        .push_notifications()
        .unwrap()
        .set_ping_interval(Duration::ZERO);
    assert!(
        fake_server
            .authenticate_account_with(
                &account,
                |push_notifications| {
                    (
                        SignalSpy::new(push_notifications.subscribe(), files_changed),
                        SignalSpy::new(push_notifications.subscribe(), notifications_changed),
                        SignalSpy::new(push_notifications.subscribe(), activities_changed),
                    )
                },
                |(
                    mut files_changed_spy,
                    mut notifications_changed_spy,
                    mut activities_changed_spy,
                )| {
                    assert!(verify_called_once_with_account(
                        &mut files_changed_spy,
                        &account
                    ));
                    assert!(verify_called_once_with_account(
                        &mut notifications_changed_spy,
                        &account
                    ));
                    assert!(verify_called_once_with_account(
                        &mut activities_changed_spy,
                        &account
                    ));
                },
            )
            .await
            .is_some()
    );
}

// Rust-only tests.

/// `--trust` applies to the websocket: a `wss` server with a self-signed
/// certificate is accepted, then authentication goes on as usual.
#[tokio::test]
async fn rust_only_wss_with_trust_invalid_certificates_authenticates() {
    let mut fake_server = FakeWebSocketServer::new_secure().await;
    let web_socket_url = fake_server.url().to_owned();
    let account = FakeWebSocketServer::create_account_with(
        "user",
        "password",
        "https://localhost",
        &web_socket_url,
        HttpClientOptions {
            trust_invalid_certificates: true,
            proxy: None,
        },
    );
    assert!(fake_server.authenticate_account(&account).await.is_some());
}

/// The account forwards the object's signals after its own, in order:
/// `pushNotificationsReady` comes before the first `filesChanged`, which is
/// what `FolderMan` relies on to connect at `ready` time.
#[tokio::test]
async fn rust_only_account_forwards_signals_in_order() {
    let mut fake_server = FakeWebSocketServer::new().await;
    let account = fake_server.create_account();
    let mut events = account.subscribe_push_notifications_events();
    assert!(fake_server.authenticate_account(&account).await.is_some());

    let mut kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        kinds.push(match event {
            AccountPushEvent::PushNotificationsReady { .. } => "pushNotificationsReady",
            AccountPushEvent::PushNotificationsDisabled { .. } => "pushNotificationsDisabled",
            AccountPushEvent::PushNotification(PushNotificationsEvent::Ready) => "ready",
            AccountPushEvent::PushNotification(PushNotificationsEvent::FilesChanged { .. }) => {
                "filesChanged"
            }
            AccountPushEvent::PushNotification(PushNotificationsEvent::NotificationsChanged {
                ..
            }) => "notificationsChanged",
            AccountPushEvent::PushNotification(PushNotificationsEvent::ActivitiesChanged {
                ..
            }) => "activitiesChanged",
            AccountPushEvent::PushNotification(_) => "other",
        });
    }
    assert_eq!(
        kinds,
        [
            "pushNotificationsReady",
            "ready",
            "filesChanged",
            "notificationsChanged",
            "activitiesChanged"
        ]
    );
}
