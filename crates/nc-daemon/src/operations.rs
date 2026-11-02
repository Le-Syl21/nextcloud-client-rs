// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The GUI's operations on the configuration, run by the engine itself
//! ([`crate::startup::ReloadHooks`]), so that a GUI attached to a running
//! `ncsyncd` and a GUI running its own engine send the same
//! [`Request`]s (decision D3 of `docs/gui-plan.md`).
//!
//! Each operation changes the configuration file with the functions
//! `ncsync` uses ([`crate::manage`]), then reloads it
//! ([`crate::startup::reload`]): the folder manager adds and removes
//! accounts and folders like the official client's `AccountManager` and
//! `FolderMan` do when its dialogs change them. Operations that wait for
//! the server run as local tasks and post [`Event::Completed`]; the event
//! loop then reloads and answers.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::account_config::{self, LimitSetting};
use crate::control::{Request, Response, TransferLimit, error};
use crate::credentials::{AppPassword, CredentialResolver, SecretStore};
use crate::event::{Completion, Event};
use crate::folder_man::FolderMan;
use crate::manage::{self, Context, RevokeOutcome};
use crate::settings::Settings;
use crate::startup::{Options, ReloadHooks};
use crate::ui_event::UiEvent;

const LOG: &str = "nextcloud.daemon.operations";

type Reply = oneshot::Sender<Response>;

/// The Login Flow v2 runs waiting for the user.
#[derive(Default)]
pub struct Logins {
    next: u64,
    running: HashMap<u64, tokio::task::AbortHandle>,
}

fn context(options: &Options) -> Context {
    Context {
        location: options.location.clone(),
        http: nc_dav::HttpClientOptions {
            trust_invalid_certificates: options.trust,
            proxy: options.http_proxy.clone(),
        },
    }
}

fn store_of(ctx: &Context) -> Option<Box<dyn SecretStore>> {
    ctx.secret_store()
        .map(|s| Box::new(s) as Box<dyn SecretStore>)
}

fn answer(reply: Reply, response: Response) {
    let _ = reply.send(response);
}

/// Runs a request the folder manager does not answer by itself.
pub fn handle(
    hooks: &mut ReloadHooks,
    fm: &mut FolderMan,
    request: Request,
    tx: &UnboundedSender<Event>,
    reply: Reply,
) {
    let ctx = context(&hooks.options);
    match request {
        Request::AddAccount {
            server_url,
            login_name,
            app_password,
        } => {
            let tx = tx.clone();
            tokio::task::spawn_local(async move {
                let store = store_of(&ctx);
                let secret = AppPassword::new(app_password.0);
                match manage::add_account(&ctx, &server_url, &login_name, &secret, store.as_deref())
                    .await
                {
                    Ok(added) => {
                        let response = json!({
                            "ok": true,
                            "account": added.account.id,
                            "existed": added.existed,
                        });
                        complete(&tx, Vec::new(), response, Some(reply), None);
                    }
                    Err(e) => answer(reply, error(e.to_string())),
                }
            });
        }
        Request::StartLogin { server_url } => start_login(hooks, fm, ctx, server_url, tx, reply),
        Request::CancelLogin { login } => match hooks.logins.running.remove(&login) {
            Some(task) => {
                task.abort();
                fm.publish(UiEvent::LoginFinished {
                    login,
                    account: None,
                    error: Some("cancelled".to_owned()),
                });
                answer(reply, json!({"ok": true, "login": login}));
            }
            None => answer(reply, error(format!("no login {login} is running"))),
        },
        Request::RemoveAccount { account, force } => {
            remove_account(hooks, fm, ctx, &account, force, reply)
        }
        Request::SignIn { account } => sign_in(hooks, fm, &ctx, &account, reply),
        Request::SignOut { account } => sign_out(fm, &ctx, &account, reply),
        Request::AddFolder {
            local_path,
            remote_path,
            account,
        } => match manage::add_folder(&ctx, &local_path, &remote_path, account.as_deref()) {
            Ok(def) => {
                let reload = hooks.reload_now(fm, &[]);
                answer(
                    reply,
                    json!({
                        "ok": true,
                        "folder": def.alias,
                        "local_path": def.local_path,
                        "remote_path": def.target_path,
                        "reload": reload,
                    }),
                );
            }
            Err(e) => answer(reply, error(e.to_string())),
        },
        Request::RemoveFolder { folder, force } => {
            remove_folder(hooks, fm, &ctx, &folder, force, reply)
        }
        Request::SetNetworkLimits {
            account,
            upload,
            download,
        } => {
            let changed = Settings::modify(&ctx.location.config_file, |s| {
                let mut acc = account_config::load_account(s, &account)?;
                let set = |limit: TransferLimit| {
                    let setting = if limit.manual {
                        LimitSetting::ManualLimit
                    } else {
                        LimitSetting::NoLimit
                    };
                    (setting, i64::from(limit.kbps))
                };
                if let Some(l) = upload {
                    let (setting, kbps) = set(l);
                    acc.set_upload_limit_setting(setting);
                    acc.upload_limit = kbps;
                }
                if let Some(l) = download {
                    let (setting, kbps) = set(l);
                    acc.set_download_limit_setting(setting);
                    acc.download_limit = kbps;
                }
                account_config::save_account(s, &acc);
                Some(())
            });
            match changed {
                Ok((Some(()), _)) => {
                    let reload = hooks.reload_now(fm, &[]);
                    answer(
                        reply,
                        json!({"ok": true, "account": account, "reload": reload}),
                    );
                }
                Ok((None, _)) => answer(reply, error(format!("no account {account}"))),
                Err(e) => answer(reply, error(e.to_string())),
            }
        }
        Request::Show => answer(reply, error("this program has no window to show")),
        r => answer(
            reply,
            error(format!("{} is not available here", r.command())),
        ),
    }
}

fn complete(
    tx: &UnboundedSender<Event>,
    wipe: Vec<String>,
    response: Response,
    reply: Option<Reply>,
    event: Option<UiEvent>,
) {
    let _ = tx.send(Event::Completed(Box::new(Completion {
        wipe,
        response,
        reply,
        event,
    })));
}

/// `Flow2Auth`: the link is answered at once, the poll goes on in a task.
fn start_login(
    hooks: &mut ReloadHooks,
    fm: &FolderMan,
    ctx: Context,
    server_url: String,
    tx: &UnboundedSender<Event>,
    reply: Reply,
) {
    hooks.logins.running.retain(|_, t| !t.is_finished());
    hooks.logins.next += 1;
    let login = hooks.logins.next;
    let tx = tx.clone();
    let ui = fm.ui_sender();
    let task = tokio::task::spawn_local(async move {
        let finished = |error: String| {
            let _ = ui.send(UiEvent::LoginFinished {
                login,
                account: None,
                error: Some(error),
            });
        };
        let flow = match manage::start_login(&ctx, &server_url).await {
            Ok(f) => f,
            Err(e) => {
                answer(reply, error(e.to_string()));
                return;
            }
        };
        answer(
            reply,
            json!({"ok": true, "login": login, "login_url": flow.login_url}),
        );
        let result = match manage::finish_login(&ctx, &flow, |e| {
            log::warn!(target: LOG, "Login {login}: {e}");
        })
        .await
        {
            Ok(r) => r,
            Err(e) => return finished(e.to_string()),
        };
        let store = store_of(&ctx);
        match manage::add_account(
            &ctx,
            &result.server_url,
            &result.login_name,
            &result.app_password,
            store.as_deref(),
        )
        .await
        {
            Ok(added) => complete(
                &tx,
                Vec::new(),
                Value::Null,
                None,
                Some(UiEvent::LoginFinished {
                    login,
                    account: Some(added.account.id),
                    error: None,
                }),
            ),
            Err(e) => finished(e.to_string()),
        }
    });
    hooks.logins.running.insert(login, task.abort_handle());
}

/// A folder by alias, or by local path (like the `folder` of the other
/// requests).
fn folder_alias(ctx: &Context, folder: &str) -> Result<String, String> {
    let settings = ctx.load().map_err(|e| e.to_string())?;
    manage::find_folder(&settings, folder)
        .map(|f| f.alias)
        .map_err(|e| e.to_string())
}

/// The daemon wipes the journals of the loaded folders it removed; the
/// others are wiped here (like `ncsync`'s `reload_after_removal`).
fn wipe_unloaded(removed: &[manage::RemovedFolder], reload: &Value) {
    for f in removed {
        let unloaded = reload["removed_folders"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == f.definition.alias.as_str()));
        if !unloaded {
            manage::wipe_journal(f);
        }
    }
}

fn wipe_list(removed: &[manage::RemovedFolder]) -> Vec<String> {
    removed
        .iter()
        .filter(|f| !f.taken_over)
        .map(|f| f.definition.alias.clone())
        .collect()
}

fn remove_folder(
    hooks: &mut ReloadHooks,
    fm: &mut FolderMan,
    ctx: &Context,
    folder: &str,
    force: bool,
    reply: Reply,
) {
    let removed = folder_alias(ctx, folder)
        .and_then(|alias| manage::remove_folder(ctx, &alias, force).map_err(|e| e.to_string()));
    match removed {
        Ok(removed) => {
            let removed = [removed];
            let reload = hooks.reload_now(fm, &wipe_list(&removed));
            wipe_unloaded(&removed, &reload);
            let [removed] = removed;
            answer(
                reply,
                json!({
                    "ok": true,
                    "folder": removed.definition.alias,
                    "taken_over": removed.taken_over,
                    "reload": reload,
                }),
            );
        }
        Err(e) => answer(reply, error(e)),
    }
}

/// `AccountManager::deleteAccount`, like `ncsync account remove`: the
/// configuration first (the engine drops the account and its folders),
/// then `deleteAppToken()` and `forgetSensitiveData()`.
fn remove_account(
    hooks: &mut ReloadHooks,
    fm: &mut FolderMan,
    ctx: Context,
    account: &str,
    force: bool,
    reply: Reply,
) {
    let store = store_of(&ctx);
    let removed = match manage::remove_account(&ctx, account, force, store.as_deref()) {
        Ok(r) => r,
        Err(e) => return answer(reply, error(e.to_string())),
    };
    let reload = hooks.reload_now(fm, &wipe_list(&removed.folders));
    wipe_unloaded(&removed.folders, &reload);
    let official = hooks
        .options
        .official_config
        .clone()
        .or_else(|| crate::config_file::official_client_config_file().ok());
    tokio::task::spawn_local(async move {
        let revoked =
            manage::revoke_app_password(&ctx, &removed, official.as_deref(), store.as_deref())
                .await;
        if let Err(e) = manage::forget_credentials(&ctx, &removed.account, store.as_deref()) {
            log::warn!(target: LOG, "Could not delete the stored credentials of account {}: {e}", removed.account.id);
        }
        let revoked = match revoked {
            RevokeOutcome::Revoked => "revoked".to_owned(),
            RevokeOutcome::SharedWithOfficialClient => "shared-with-official-client".to_owned(),
            RevokeOutcome::NoPassword => "no-password".to_owned(),
            RevokeOutcome::Failed(status) => format!("failed ({status})"),
        };
        answer(
            reply,
            json!({
                "ok": true,
                "account": removed.account.id,
                "folders": removed
                    .folders
                    .iter()
                    .map(|f| f.definition.alias.as_str())
                    .collect::<Vec<_>>(),
                "app_password": revoked,
                "reload": reload,
            }),
        );
    });
}

/// `AccountState::signIn()`, then the credentials are read from where they
/// are stored: found, the account connects with them; missing, it asks
/// for new ones ([`UiEvent::CredentialsNeeded`]).
fn sign_in(hooks: &ReloadHooks, fm: &mut FolderMan, ctx: &Context, account: &str, reply: Reply) {
    if !fm.sign_in(account) {
        return answer(reply, error(format!("no account {account}")));
    }
    let def = ctx
        .load()
        .ok()
        .and_then(|s| account_config::load_account(&s, account));
    let store = store_of(ctx);
    let resolver = CredentialResolver::new(hooks.options.location.mode.clone(), store.as_deref());
    let found = def.and_then(|d| match resolver.app_password(&d) {
        Ok((password, _)) => Some(nc_dav::Credentials::new(
            d.credentials_user(),
            password.expose(),
        )),
        Err(e) => {
            log::info!(target: LOG, "Account {account}: {e}");
            None
        }
    });
    let has_credentials = found.is_some();
    if let Some(credentials) = found {
        fm.set_account_credentials(account, credentials);
    }
    answer(
        reply,
        json!({"ok": true, "account": account, "credentials": has_credentials}),
    );
}

/// `AccountState::signOutByUi()`: `forgetSensitiveData()` also deletes the
/// stored app password (`HttpCredentials::invalidateToken`).
fn sign_out(fm: &mut FolderMan, ctx: &Context, account: &str, reply: Reply) {
    if !fm.sign_out(account) {
        return answer(reply, error(format!("no account {account}")));
    }
    let def = ctx
        .load()
        .ok()
        .and_then(|s| account_config::load_account(&s, account));
    if let Some(def) = def {
        let store = store_of(ctx);
        if let Err(e) = manage::forget_credentials(ctx, &def, store.as_deref()) {
            log::warn!(target: LOG, "Could not delete the stored credentials of account {account}: {e}");
        }
        // A password file of ours is gone: the account must not point to it.
        if def
            .password_file
            .as_deref()
            .is_some_and(|p| !Path::new(p).exists())
        {
            let mut def = def;
            def.password_file = None;
            let _ = Settings::modify(&ctx.location.config_file, |s| {
                account_config::save_account(s, &def)
            });
        }
    }
    answer(reply, json!({"ok": true, "account": account}));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_config::{AccountDefinition, save_account};
    use crate::config_file::{ConfigLocation, ServiceMode};
    use crate::folder_man::FolderManSettings;

    async fn ask(tx: &UnboundedSender<Event>, request: Request) -> Value {
        let (otx, orx) = oneshot::channel();
        tx.send(Event::Control(request, otx)).unwrap();
        orx.await.unwrap()
    }

    #[test]
    fn derived_operations_change_the_configuration_and_reload_it() {
        let dir = tempfile::tempdir().unwrap();
        let location = ConfigLocation {
            mode: ServiceMode::System {
                instance: "test".to_owned(),
            },
            config_file: dir.path().join("ncsyncd.cfg"),
            state_dir: dir.path().join("state"),
        };
        let options = Options {
            location: location.clone(),
            trust: false,
            http_proxy: None,
            no_watch: true,
            no_push: true,
            official_config: Some(dir.path().join("no-official.cfg")),
        };
        let pw = dir.path().join("pw0");
        std::fs::write(&pw, "secret\n").unwrap();
        let mut acc = AccountDefinition::new_webflow("0", "http://127.0.0.1:9", "alice");
        acc.password_file = Some(pw.to_string_lossy().into_owned());
        Settings::modify(&location.config_file, |s| save_account(s, &acc)).unwrap();
        let local = dir.path().join("A").to_string_lossy().into_owned();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut fm = FolderMan::new(
                &tx,
                FolderManSettings::default(),
                Box::new(crate::folder_man::NullSettingsStore),
                crate::folder_man::no_watcher(),
            );
            crate::startup::setup(&mut fm, &options).unwrap();
            let mut hooks = ReloadHooks::new(options.clone());
            let driver = {
                let tx = tx.clone();
                let cfg = location.config_file.clone();
                async move {
                    let status = || async {
                        crate::ui_event::Snapshot::from_response(&ask(&tx, Request::Status).await)
                            .unwrap()
                    };
                    // add-folder: the local folder is created and loaded
                    let r = ask(
                        &tx,
                        Request::AddFolder {
                            local_path: local.clone(),
                            remote_path: "/".into(),
                            account: None,
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], true, "{r}");
                    let alias = r["folder"].as_str().unwrap().to_owned();
                    assert_eq!(r["reload"]["added_folders"][0], alias.as_str());
                    assert!(Path::new(&local).is_dir());
                    assert_eq!(status().await.folders.len(), 1);

                    // set-network-limits: written and reloaded
                    let r = ask(
                        &tx,
                        Request::SetNetworkLimits {
                            account: "0".into(),
                            upload: Some(TransferLimit {
                                manual: true,
                                kbps: 100,
                            }),
                            download: None,
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], true, "{r}");
                    let s = Settings::load(&cfg).unwrap();
                    let a = account_config::load_account(&s, "0").unwrap();
                    assert_eq!(a.upload_limit_setting, LimitSetting::ManualLimit);
                    assert_eq!(a.upload_limit, 100);
                    assert_eq!(a.download_limit_setting, LimitSetting::NoLimit);

                    // sign-out, sign-in (the administrator's password file
                    // is left in place, so the credentials are found again)
                    let r = ask(
                        &tx,
                        Request::SignOut {
                            account: "0".into(),
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], true, "{r}");
                    assert!(status().await.accounts[0].signed_out);
                    let r = ask(
                        &tx,
                        Request::SignIn {
                            account: "0".into(),
                        },
                    )
                    .await;
                    assert_eq!(r["credentials"], true, "{r}");
                    assert!(!status().await.accounts[0].signed_out);
                    let r = ask(
                        &tx,
                        Request::SignIn {
                            account: "9".into(),
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], false);

                    // remove-account is refused while it has folders
                    let r = ask(
                        &tx,
                        Request::RemoveAccount {
                            account: "0".into(),
                            force: false,
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], false);

                    // remove-folder by local path: unloaded, files kept
                    let r = ask(
                        &tx,
                        Request::RemoveFolder {
                            folder: local.clone(),
                            force: false,
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], true, "{r}");
                    assert_eq!(r["folder"], alias.as_str());
                    assert!(status().await.folders.is_empty());
                    assert!(Path::new(&local).is_dir());

                    // remove-account: gone from the engine (the server
                    // cannot be reached to revoke the app password)
                    let r = ask(
                        &tx,
                        Request::RemoveAccount {
                            account: "0".into(),
                            force: false,
                        },
                    )
                    .await;
                    assert_eq!(r["ok"], true, "{r}");
                    assert_eq!(r["app_password"], "failed (0)");
                    assert!(status().await.accounts.is_empty());

                    let r = ask(&tx, Request::Show).await;
                    assert_eq!(r["ok"], false);
                    tx.send(Event::Shutdown).unwrap();
                }
            };
            tokio::join!(
                crate::daemon::run(&mut fm, &tx, &mut rx, &mut hooks),
                driver
            );
        });
    }
}
