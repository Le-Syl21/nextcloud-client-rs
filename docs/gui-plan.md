# GUI and multi-OS plan

Status: study, 2026-11-02. P0 and P1 (§9) are implemented, P2 is largely
implemented (2026-11-03): see the notes there for what is left.

Goal: a real multi-OS (Linux, Windows, macOS) desktop client. A new GUI
binary built with egui/eframe 0.37 on the glow backend, with a light, dark or
automatic theme and a system tray icon that shows the sync state. Like the
official client, the GUI runs the sync engine **in its own process**.
`ncsyncd` stays as the headless server daemon. Both use the same
`nextcloud.cfg`-format configuration, and a guard stops both from running on
the same configuration. Packaging ships two downloads per OS: `-cli`
(`ncsync` + `ncsyncd`) and `-gui`.

The visual design is our own, not a pixel copy. Behaviour follows the
official client: the same screens and actions, the same state logic, the same
configuration keys.

Reference: [nextcloud/desktop](https://github.com/nextcloud/desktop) tag
**v34.0.5** (commit `62ebad6043b1e7c8e319f41be25d41f5a2c733e5`).
`UP/` below means that tree, for example a shallow clone at
`~/.cache/claude-work/nextcloud-desktop-34.0.5`. Crate versions were checked
on crates.io on 2026-11-02.

Contents:

1. [Summary and recommended decisions](#1-summary-and-recommended-decisions)
2. [Architecture](#2-architecture)
3. [Inventory of the official GUI](#3-inventory-of-the-official-gui)
4. [Tray behaviour](#4-tray-behaviour)
5. [Desktop notifications](#5-desktop-notifications)
6. [Linux-only code in our crates, and how to port it](#6-linux-only-code-in-our-crates-and-how-to-port-it)
7. [Upstream tests that become applicable](#7-upstream-tests-that-become-applicable)
8. [Crate choices for the GUI](#8-crate-choices-for-the-gui)
9. [Phased implementation plan](#9-phased-implementation-plan)
10. [Decisions](#10-decisions)

---

## 1. Summary and recommended decisions

### What the official v34 GUI really is

v34 changed the shape of the client, and the plan follows that shape:

- **There is no tray window any more.** `Window.qml` and `MainWindow.qml`
  are gone. The tray opens a **native menu**: a `QMenu` on Linux and
  Windows (`UP/src/gui/trayaccountpopup_qt.cpp`), a borderless `NSPanel` on
  macOS (`UP/src/gui/macOS/trayaccountpopup/`). It has one submenu per
  account, then Add account, Pause or Resume all, Settings and Quit.
- What the tray window used to show is now in separate windows, one per
  account: **Activities** (sync status bar plus the activity list),
  **Search**, **Assistant** and **User status**.
- **The "main dialog" is the Settings dialog** (`Systray::showWindow()`
  only emits `openSettings()`). Its sidebar lists the accounts, then
  "Add account", then **General / Advanced / Info**.
- **The account wizard is QML only** (`UP/src/gui/wizard/`). The old
  QWizard pages are no longer compiled.
- **Proxy and bandwidth settings are per account** (a "Connection settings"
  dialog opened from the account page). There is no Network tab any more.

A native tray menu maps well onto Rust tray crates (StatusNotifierItem +
DBusMenu on Linux, Win32 menus, NSMenu). It also avoids the problem upstream
has on Wayland, where a positioned popup cannot be created.

### Recommended decisions

| Topic | Recommendation |
|---|---|
| GUI toolkit | `eframe` 0.37 with `glow`, `persistence` and `accesskit`; `egui` 0.37 with `monochrome_emoji_fonts`; the theme, zoom and i18n patterns of PinReady and SSR. |
| Window model | **One main window** with a sidebar: per-account Activity, per-account Account, General, Advanced, About. Dialogs are egui modals inside it. "Hide to tray" uses `CancelClose` + `Visible(false)`: eframe 0.37 keeps running `App::logic()` while the window is hidden, so tray events are still handled. Upstream's separate windows become pages. This is a layout choice, not a functional change. |
| Tray | **`tray-icon` 0.26.1** on all three OS. On Linux use `default-features = false, features = ["ksni"]`: StatusNotifierItem over D-Bus on its own worker thread, **with no GTK and no libappindicator**. On Windows and macOS, create it on the eframe main thread, where winit already runs the message loop. Menus come from `muda` 0.21. |
| Engine embedding | The engine runs on **its own thread** with the existing current-thread tokio runtime + `LocalSet` (`FolderMan` is `!Send`). The GUI sends `Event::Control` requests on the existing channel and receives a **new `UiEvent` broadcast** (folder state, progress, completed items, errors, prompts). Each event calls `ctx.request_repaint()`. |
| ncsyncd / GUI guard | An **exclusive OS file lock** per configuration file (`std::fs::File::try_lock`, stable since Rust 1.89; `fs4` 1.1.0 otherwise), taken **before** `startup::setup`. The control endpoint name is derived from the same hash. The OS releases the lock on a crash, so there is no stale-lock logic. |
| Single instance (GUI) | The same lock. A second GUI launch connects to the control endpoint, sends `Show` and exits, which is what upstream's KDSingleApplication does. |
| Control endpoint | **`interprocess` 2.4.7** (`tokio` feature): Unix sockets on Linux and macOS, named pipes on Windows with an owner-only security descriptor. |
| Platform code | A new **`nc-platform`** crate. It is the only crate with `#![allow(unsafe_code)]` and gives a safe API over the Win32 and Apple calls that have no safe wrapper (hidden and read-only attributes, ACLs, `ReadDirectoryChangesW`, `GetVolumeInformationW`, Restart Manager, FSEvents). It mirrors upstream's `filesystembase.cpp`, `utility_win.cpp` and `utility_mac.mm`. The rest of the workspace keeps `unsafe_code = "deny"`. |
| Folder watcher | Port **`folderwatcher_win.cpp`** (40 to 64 KiB buffer, `ERROR_NOTIFY_ENUM_DIR` → lost changes) and **`folderwatcher_mac.cpp`** (FSEvents flags, NFC) the way inotify was ported. `notify` 8.2.1 is not faithful on Windows: it uses a fixed 16 KiB buffer and gives no lost-changes signal. |
| Keychain | `keyring-core` 1.0.x with per-target stores: `windows-native-keyring-store` 1.1.0, `apple-native-keyring-store` 1.0.2 (feature `keychain`), `zbus-secret-service-keyring-store` 1.0.1. Reading the official client's entry during takeover goes through `get_secret()` (raw UTF-8), never `get_password()`. |
| Notifications | `notify-rust` 4.18 (Linux, pure zbus) and `tauri-winrt-notification` 0.8.1 (Windows toasts; needs an AppUserModelID on a Start-menu shortcut). On macOS, `notify-rust` (via `mac-notification-sys`) for v1, then `UNUserNotificationCenter` through `objc2-user-notifications` in `nc-platform`. |
| Autostart | Our own small module, faithful to upstream: the HKCU `Run` key (`windows-registry` 0.100), an XDG autostart `.desktop` file, and a macOS LaunchAgent plist (upstream uses `SMAppService`, which needs a bundled, signed app; that can come later via `objc2-service-management`). |
| Open URL / folder | `opener` 0.9 with `reveal`: browser, file manager, and "show in folder" with the file selected. |
| File dialogs | `rfd` 0.17 with `xdg-portal` (no GTK), as SSR does. |
| GUI tests | `egui_kittest` 0.37 in CI on all three OS (AccessKit queries, no GPU), plus `egui_mcp` against the live app: Linux in an Xvfb container (as for clap-adn and SSR), Windows in the GPU-passthrough VM. |

Section 10 lists what needs Sylvain's decision: the product name and
binaries, the configuration directory, whether the GUI may attach to a
running ncsyncd, how macOS gets tested, reuse of the official translations,
and installers.

---

## 2. Architecture

### 2.1 Crates

```
nc-journal  nc-dav  nc-sync          (engine, unchanged API; platform fixes from §6)
nc-platform                          NEW: safe wrappers over Win32 / Apple APIs (unsafe allowed here only)
nc-daemon                            FolderMan, accounts, config, control, takeover (+ UiEvent bus, guard)
ncsync      ncsyncd                  CLI and headless daemon (unchanged roles)
nc-gui                               NEW: library with the egui app (screens, tray, notifications)
nc-client                            NEW: thin main() of the GUI (§10, D1)
```

Keeping the GUI in a library crate lets `egui_kittest` tests drive the real
screens without a window.

### 2.2 Threads and data flow

```
 main thread (winit / eframe)                    engine thread (tokio current-thread + LocalSet)
 ─────────────────────────────                   ────────────────────────────────────────────────
 App::logic()  ← tray / menu events (crossbeam)  FolderMan::handle_event(Event)
 App::ui()     → egui screens                    nc-sync engines, watchers, push, timers
   │   Event::Control(Request, oneshot) ───────▶ control::handle (Pause/Resume/SyncNow/...)
   │   GuiRequest (add account, add folder, …) ▶ manage.rs / folder_man.rs
   ◀── broadcast<UiEvent> + ctx.request_repaint()  FolderMan emits UiEvent
 tray-icon (Win/mac: main thread; Linux: ksni worker thread)
```

- Today `nc-daemon/src/daemon.rs:165-252` `run()` already takes the event
  channel and hooks. It must lose its headless-only parts: `sd_notify`, the
  watchdog and the signal handlers move into ncsyncd-only `DaemonHooks`.
- **Inbound** already exists: `Event::Control(control::Request,
  oneshot::Sender<Response>)` with `Status`, `Pause`, `Resume`, `SyncNow`,
  `Reload` and `SelectiveSync` (`control.rs:37-75`). These are serde types,
  so the GUI can send them in-process, with no socket. They need extending
  with the GUI's operations: add or remove an account, add or remove a
  folder, sign in or out, network limits, accept a prompt.
- **Outbound is missing.** Engine signals (`FolderEvent::{EngineStarted,
  ItemCompleted, TransmissionProgress, SyncError, NewBigFolder,
  ExistingFolderNowBig, AllFilesDeletedRestore, LockFileDetected, …}`,
  `event.rs:62-104`) are consumed inside `FolderMan` and nothing goes out.
  Add a `tokio::sync::broadcast<UiEvent>`, or an observer trait, fed from
  `FolderMan::handle_event`, with:
  `FolderSyncStateChanged`, `Progress` (a summary of `ProgressInfo`),
  `ItemCompleted` (feeds the activity list), `SyncError`,
  `AccountStateChanged`, `ConflictsChanged`, `QuotaChanged`,
  `CredentialsNeeded`, and prompts that carry a reply channel:
  `NewBigFolder`, `AboutToRemoveAllFiles`, `SslErrors`,
  `ProxyAuthRequired`.
- **An existing bug to fix first:** `FolderMan::take_requests()`
  (`folder_man.rs:181`) is never called, so
  `ManagerRequest::CredentialsNeeded` is silently dropped today, in ncsyncd
  too. The GUI needs it to open the re-authentication window.
- The mass-deletion and big-folder prompts are answered from the
  configuration today (ncsyncd has no user to ask). In the GUI they become
  modal dialogs, as in upstream `Folder::slotAboutToRemoveAllFiles`
  (`UP/src/gui/folder.cpp:1786`).
- Logs: ncsyncd logs to stderr/journald. The GUI needs upstream's log
  behaviour: a temporary log directory, one file per sync run, compression,
  expiry (`logToTemporaryLogDir`). Either write a small `log::Log` backend
  that reproduces it, or use `flexi_logger` 0.31.10.

### 2.3 Configuration and the guard

- The GUI and ncsyncd read and write the same file format (`nextcloud.cfg`
  groups and keys, through `ini-preserve`). Which file the GUI uses is
  decision D2 (§10): the ncsyncd file, so the GUI can take over from the
  daemon, or attach to a running one (D3).
- Guard (shared by both programs, in `nc-daemon`):
  1. Canonicalize the configuration path and hash it (md5, as the journal
     name is made).
  2. Open `<state_dir>/instance-<hash>.lock` and **`try_lock()`** it
     (flock on Unix, `LockFileEx` on Windows). Keep it open for the whole
     life of the process. Write `pid` and the program name into it so the
     refusal message can say who holds it.
  3. Do this **before** `startup::setup` (today the socket check in
     `control::serve` runs after setup and is per user, not per config:
     `control.rs:254-261`, `ncsyncd/src/main.rs:177-184`).
  4. Name the control endpoint after the same hash
     (`$XDG_RUNTIME_DIR/ncsyncd/<hash>.sock`, `\\.\pipe\ncsyncd-<user>-<hash>`,
     macOS: under `dirs::data_local_dir()`, keeping `sun_path` under 104
     bytes).
  5. If the lock is held by another GUI, send `Show` (single instance). If
     ncsyncd holds it, the GUI attaches to it as a remote client over the
     control endpoint (decision D3): same pages, same operations, the
     daemon does the work.
- The journal's `PRAGMA locking_mode=EXCLUSIVE` stays as the last line of
  defence, and the refusal of folders that the official client also lists
  (`startup.rs:221-237,343-358`) stays.

---

## 3. Inventory of the official GUI

Classification: **MUST** (v1), **LATER** (wanted after v1), **N/A** (outside
our scope: VFS, E2EE, macOS File Provider and sandbox, shell extensions and
socket API, Windows Explorer navigation pane).

### 3.1 Summary table

| # | Screen | Upstream sources | Class |
|---|---|---|---|
| 1 | Tray menu (popup) | `src/gui/trayaccountpopup_qt.cpp`, `src/gui/tray/trayaccountmenupolicy.h`, `tray/usermodel.cpp`, `tray/trayaccountappsmodel.cpp`, macOS `src/gui/macOS/trayaccountpopup/*.mm` | MUST |
| 2 | Tray context menu | `src/gui/systray.cpp` `setupContextMenu` (l.575) | MUST |
| 3 | Activities window (status bar + activity list) | `src/gui/activity/qml/*`, `activitylistmodel.cpp`, `syncstatussummary.cpp`, `notificationhandler.cpp`, `WindowAccountHeader.qml` | MUST (Talk reply, file-details menu: LATER) |
| 4 | Settings dialog (container) | `settingsdialog.cpp`, `settingspanelstyle.cpp`, `settingsswitch.cpp` | MUST |
| 4.1 | Account page (folders, selective sync inline, status, quota, SSL button, log in/out, remove) | `accountsettings.cpp/.ui`, `folderstatusmodel.cpp`, `folderstatusdelegate.cpp`, `folderstatusview.cpp`, `tooltipupdater.cpp`, `sslbutton.cpp` | MUST (VFS, E2EE, File Provider panels: N/A) |
| 4.2 | General page | `generalsettings.cpp/.ui` | MUST |
| 4.3 | Advanced page | `advancedsettings.cpp/.ui` | MUST |
| 4.4 | Info page (about, legal, updater) | `infosettings.cpp/.ui`, `updater/*`, `legalnotice.cpp` | MUST (updater: LATER) |
| 4.5 | Connection settings (per-account proxy and bandwidth) | `networksettings.cpp/.ui`, `accountmanager.cpp` l.49-57 | MUST |
| 5 | Account wizard (server, Login Flow v2, basic auth, sync options, advanced, proxy, client certificate) | `wizard/accountwizardcontroller.cpp`, `wizard/qml/*`, `owncloudsetupwizard.cpp` | MUST (virtual-files option: N/A) |
| 5b | Browser re-auth window | `wizard/browserreauthwindow.cpp`, `browserreauthcontroller.cpp`, `qml/BrowserReAuthWindow.qml` | MUST |
| 5c | HTTP password prompt (basic-auth accounts) | `creds/httpcredentialsgui.cpp` | MUST |
| 6 | Folder wizard ("Add Folder Sync Connection") | `folderwizard.cpp`, `folderwizardsourcepage.ui`, `folderwizardtargetpage.ui` | MUST |
| 7 | Selective sync dialog | `selectivesyncdialog.cpp` | MUST |
| 8 | Conflict dialog (one file) and Resolve conflicts (many) | `conflictdialog.cpp/.ui`, `conflictsolver.cpp`, `ResolveConflictsDialog.qml`, `ConflictDelegate.qml`, `syncconflictsmodel.cpp` | MUST |
| 9 | Invalid filename dialog | `invalidfilenamedialog.cpp/.ui` | MUST |
| 9b | Case-clash filename dialog | `caseclashfilenamedialog.cpp/.ui` | MUST on Windows/macOS (case-insensitive file systems), LATER on Linux |
| 10 | SSL error dialog "Untrusted Certificate" | `sslerrordialog.cpp/.ui` | MUST |
| 11 | Ignore list editors (global and per folder) | `ignorelisteditor.cpp/.ui`, `ignorelisttablewidget.cpp/.ui` | MUST |
| 12 | Log browser | `logbrowser.cpp` | MUST |
| 13 | Legal notice | `legalnotice.cpp/.ui` | MUST |
| 14 | Small dialogs: create folder, proxy auth, "Remove all files?", config downgrade, config access error, unsupported server / terms of service, logger messages | `foldercreationdialog.*`, `proxyauthdialog.*`, `proxyauthhandler.cpp`, `folder.cpp` l.1786, `application.cpp` l.202/l.840, `owncloudgui.cpp` l.301-324 | MUST |
| 14b | Legacy import / account selection | `accountmanager.cpp` l.257, `legacyaccountselectiondialog.cpp` | LATER (overlaps with our takeover) |
| 15 | User status window | `UserStatusWindow*.qml`, `EmojiPicker.qml`, `userstatusselectormodel.cpp` | LATER |
| 16 | Unified search window | `src/gui/search/*` | LATER |
| 17 | Assistant window | `AssistantWindow.qml` | LATER |
| 18 | File details / sharing | `src/gui/filedetails/*`, `sharemodel.cpp`, `shareemodel.cpp`, `ocssharejob.cpp`, `sharemanager.cpp` | LATER |
| 19 | File actions window, socket API shell integration, FinderSync, File Provider | `integration/FileActionsWindow.qml`, `socketapi/*`, `shellextensionsserver.cpp`, `macOS/*` | N/A in v1 (LATER with file-manager extensions) |
| 20 | Talk call notification popup (ringtone) | `tray/CallNotificationDialog.qml`, `callstatechecker.cpp`, `notificationsoundplayer*.cpp` | LATER |
| 21 | Edit locally (`nc://open/…`) | `editlocally*.cpp`, `urischemehandler.cpp`, `tray/EditFileLocallyLoadingDialog.qml` | LATER (the `nc://login` URI is close to MUST: §3.4) |
| 22 | Governance labels | `GovernanceLabelsDialog.qml`, `governance/*` | LATER / N/A |
| 23 | Updater dialogs | `updater/ocupdater.cpp` (Sparkle on macOS) | LATER |
| 24 | Remote wipe (no screen) | `remotewipe.cpp` | MUST (already ported in `nc-daemon/src/remote_wipe.rs`) |
| 25 | libcloudproviders export (GNOME Files) | `src/gui/cloudproviders/*` | LATER |
| — | VFS dialogs, E2EE (mnemonic, encryption token), `PasswordInputDialog`, `AuthenticationDialog` (no caller) | `vfsdownloaderrordialog.*`, `mnemonicdialog.ui`, `tray/EncryptionTokenDiscoveryDialog.qml` | N/A |

### 3.2 Tray menu and context menu (MUST)

The popup is 320 px wide. Long texts are elided, with the full text in a
tooltip.

**Top level, in order:**

1. **One submenu per account.**
   - Label `"<display name> (<server>)"`, elided in the middle.
   - Icon: that account's sync-state icon.
   - Rebuilt on `aboutToShow`; opening it fetches an activity preview.
2. "**Add account**". Hidden when single-account branding already has an
   account.
3. **Global sync control** (`Systray::syncControlState()`):
   - "Pause sync for all" when every folder runs;
   - "Resume sync for all" when every folder is paused;
   - both when mixed;
   - nothing when there are no folders.
4. "**Settings**".
5. "**Quit**".

**Account submenu, connected account:**

- "Resolve: <alert>" when there is an account alert: sync conflicts,
  reauthorization required, terms of service. It opens Activities.
- "User status" heading plus the current status. Only with the
  `user_status` capability (LATER for us: show it read-only, or hide it).
- "**Local folder**" ("Reveal in Finder" on macOS).
- "Assistant" if the server has it (LATER), "Search" (LATER).
- "**Apps**" submenu: the server's navigation apps (OCS
  `ocsnavigationappsjob`) with their icons, opened in the browser. When
  there are none: a disabled "No apps available".
- "**Notifications**" section: one entry per server notification,
  `"title: subtitle - <date>"`. If the notification has actions, a submenu
  with "Open" plus the server actions; "dismiss" calls
  `dismissNotification`.
- "**Recent activity**": up to 5 entries, or a disabled "No recent
  activity". Then "**More activity…**", which opens Activities.

**Account submenu, disconnected account:** "Local folder", and "**Log in**"
unless it is a public-share account.

**Context menu:**

- Linux: the same content as the popup.
- Windows: a short classic menu: "Add account" or "Open %1 Desktop",
  Pause/Resume (all), Settings, Help (opens `Theme::helpUrl`), "Exit %1".
- macOS: none; a right-click opens the same popup as a left-click.

### 3.3 Activities window (MUST)

One per account. Header: account avatar, name, server.

**Sync status bar** (`syncstatussummary.cpp`):

- Icon: ok, sync, pause, warning, error or offline.
- Status text: "All synced!", "Syncing changes", "Checking folder changes",
  "Syncing file %1 of %2", "Sync paused", "Some files couldn't be synced!",
  "Offline", "No synchronisation configured", "Reauthorization required".
- A progress bar while syncing.
- Detail text: "%1 of %2 · %3 left", "See below for errors/warnings",
  "You need to accept the terms of service".
- Buttons:
  - **Sync now**: shown when there is no conflict, no sync is running, and
    the account has a folder and is connected;
  - **Resolve conflicts**: when there are conflicts;
  - **Open browser**: when the terms of service must be accepted.

**Activity list** (`ActivityListModel`). Row types:

- server notifications (OCS);
- server activities (`ocs/v2.php/apps/activity/api/v2/activity`, fetched
  only while an activity surface is visible);
- local sync errors, conflicts, ignored and invalid files;
- the quota warning (opens Settings).

Each row shows an icon or thumbnail, the subject or message, a relative
time, "In <folder>", the server-defined action buttons (others in a context
menu), and **Dismiss** for notifications (OCS DELETE). There is also a
file-details menu, and a Talk inline reply (both LATER).

**Default click action** (`activitylistmodel.cpp` l.911):

- conflict → the conflict dialog for one file, or Resolve conflicts if
  there are several;
- case clash → the case-clash dialog;
- invalid name → the invalid-filename dialog;
- quota → Settings;
- otherwise → the local file, or else the link.

### 3.4 Settings (MUST)

**Container.**

- Sidebar: accounts (avatar + short name), "Add account", then General,
  Advanced, Info.
- Ctrl+W closes. **F12 or Ctrl+L toggles the log browser**.
- The geometry is saved (`[Settings] geometry`).
- When the last account is removed, the dialog hides and the wizard opens.

**Account page, folder list.**

*Root folder row:*

- the remote path (or the app name for `/`) and the local path;
- the status icon and text;
- while syncing: a progress bar, the overall text ("%5 left, %1 of %2,
  file %3 of %4", "Waiting for %n other folder(s)", "Preparing to sync",
  "Checking for changes in remote/local") and the current item ("Uploading
  foo, ↑ 1 MB/s");
- errors in red (a click opens Activities);
- "There are unresolved conflicts. Click for details.";
- a live tooltip (`ToolTipUpdater`);
- a "…" button that opens the context menu;
- a click expands the subfolder tree.

*Subfolders (inline selective sync):*

- tri-state checkboxes with "name (size)";
- once something is unchecked, a bar explains that unchecked folders will
  be **removed** locally, with **Cancel / Apply**;
- "Fetching folder list from server…" while loading, or the load error.

*Under the list:*

- an **Add Folder Sync Connection** button, disabled when disconnected;
- the big-folder and external-storage bar, with **Synchronize all /
  Synchronize none / Apply manual changes**.

*Root folder context menu* (`accountsettings.cpp` l.753):

- **Open folder**;
- **Edit Ignored Files** (`<folder>/.sync-exclude.lst`);
- **Create new folder**;
- **Choose what to sync**;
- **Force sync now** (labelled "Restart sync" while a sync runs);
- **Pause / Resume sync** (asks first if a sync is running);
- **Remove folder sync connection**, with the confirmation "This will not
  delete any files".

The VFS entries are N/A.

*Subfolder context menu:* Open folder, Edit Ignored Files, Create new
folder. Encrypt (E2EE) and Availability (VFS) are N/A.

*Shortcuts:* **F6** schedules the selected folder. **Ctrl+F6** clears its
blacklist, forces remote discovery, then schedules it.

**Account page, status panel.**

- SSL button: cipher tooltip and a menu with the server version and the
  certificate chain, each certificate with its details.
- Connection label, one of:
  - "Connected to <server> as <user>. (X of Y in use)" (the quota is
    shown only here in v34);
  - temporarily unavailable, maintenance mode, redirected or captive
    portal;
  - "Signed out", "Connecting";
  - "Unable to connect" plus the errors;
  - server configuration error, terms of service, unsupported version.
- A **Connection settings** button.

**Account page, actions:** **Log out / Log in** and **Remove account**
("Leave share" for a share account). The confirmation says the files are
not deleted. In v34 there is no "edit settings" or "fetch apps" button.

**General** (`[General]` keys):

| UI | Key | Default |
|---|---|---|
| Launch on system startup | `launchOnSystemStartup` + the OS autostart | turned on by the wizard |
| Use monochrome icons (tray) | `monoIcons` | false (true on macOS for the Nextcloud brand) |
| Show server notifications | `optionalServerNotifications` | true; the three switches below need it |
| Show chat notifications | `showChatNotifications` | true |
| Show call notifications | `showCallNotifications` | true |
| Show quota warning notifications | `showQuotaWarningNotifications` | true (80 %) |
| (macOS) Enable File Provider | `macFileProviderModeEnabled` | N/A |

Upstream has no language selector in the GUI (only `--language` and the
`language` key). We add one, as in our other apps, and add the theme
(light, dark, auto) and zoom. These are our own keys, in our own group,
so they never collide with upstream keys.

**Advanced:**

| UI | Key | Default |
|---|---|---|
| Ask before syncing new folders larger than N MB | `useNewBigFolderSizeLimit`, `newBigFolderSizeLimit` | true, 500 |
| Notify when synced folders grow over the limit | `notifyExistingFoldersOverLimit` | false |
| Stop syncing folders that grow over the limit | `stopSyncingExistingFoldersOverLimit` | follows the previous key |
| Server poll interval (hidden when notify_push is available) | `remotePollInterval` (ms, group `[Nextcloud]`) | 30000, minimum 5 s |
| Ask before syncing external storages | `confirmExternalStorage` | true |
| Move removed files to trash | `moveToTrash` | false |
| Show sync folders in Explorer's navigation pane | `showInExplorerNavigationPane` | Windows only: LATER |
| Edit Ignored Files | global `sync-exclude.lst` in the config dir | — |
| Create Debug Archive | zip of the logs, config, journals (+wal/shm) and build info | — (`zip` 9.2.2) |

Several of these keys can be overridden by Windows policies
(`HKCU/HKLM\Software\Policies\<vendor>\<app>`): LATER.

**Info:**

- the about text (with our own wording: an unofficial client);
- Usage documentation;
- Legal notice;
- the updater group (channel, auto check, Check now, Restart & update):
  LATER.

**Connection settings (per account):**

- Proxy: none, system, or manual (HTTP(S)/SOCKS5, host, port, optional
  user and password). The proxy password goes to the keychain under the
  `_proxy_password` suffix.
- Upload and download limits: none, or N KB/s.
- Keys in `[Accounts]`: `networkProxyType`, `networkProxyHostName`,
  `networkProxyPort`, `networkProxyNeedsAuth`, `networkProxyUser`,
  `networkUploadLimitSetting`, `networkDownloadLimitSetting`,
  `networkUploadLimit`, `networkDownloadLimit`. The legacy global `Proxy/*`
  and `BWLimit/*` keys are migrated (`migrateNetworkSettings`).
- `nc-daemon/src/network_limits.rs` already reads the limits.

### 3.5 Account wizard (MUST)

Steps `ServerStep → BrowserAuthStep | BasicAuthStep → SyncOptionsStep →
CompletedStep` (`accountwizardcontroller.cpp`). It opens:

- at startup without accounts;
- from "Add account";
- from a tray click with no account;
- from the `nc://login` URI;
- after the last account is removed.

If it is already open, it is brought to the front.

**Server page.**

- "Log in to %1", a server address field (or a list of predefined
  servers), "Log in", the error text, and "Checking server address…".
- Footer: Sign up, Self-host, **Proxy settings**.
- If TLS fails: "Connect to %1?" with **Cancel / Connect without TLS / Use
  client certificate** (a PKCS#12 file + password).
- Untrusted certificates go through the SSL error dialog.
- `DetermineAuthTypeJob` then picks Login Flow v2 or basic auth.

**Browser page (Login Flow v2).**

- "Switch to your browser", a busy indicator, and "Waiting for
  authorization…" / "Link copied to clipboard".
- Buttons Cancel / Copy link / Open.
- `nc-daemon/src/flow2auth.rs` already implements the flow.

**Basic auth page.** User and password, or the share password for a public
link. Errors: invalid WebDAV response, redirect, forbidden.

**Sync options page.**

- Avatar, name and server.
- "Synchronize everything" (with the size needed) or "Choose what to sync"
  (opens the selective sync dialog). The "Virtual files" choice is N/A.
- The local folder with Choose and the free space. Errors: not enough
  space, folder not chosen, folder not empty.
- Buttons Back, **Set up later** (account without a folder), **Advanced**
  (the big-folder limit and the external-storage confirmation), and Done.

**On finish:**

1. create the remote folder if needed;
2. create the local folder;
3. `addFolder`;
4. write the blacklist, and `"/"` to the whitelist;
5. turn autostart on;
6. show the tray menu.

`nc-daemon/src/account_setup.rs` already does most of this for
`ncsync sync --userid`.

**Re-auth window.** It opens when a Login Flow token is rejected ("You have
been logged out of your account %1 at %2"), and reuses the browser page.

### 3.6 Folder wizard and selective sync (MUST)

**Folder wizard**, three pages:

1. The local folder, validated by `FolderMan::checkPathValidityForNewFolder`.
   Warnings are shown in a yellow box.
2. The remote folder: a PROPFIND tree, a path entry, **Create folder**
   (MKCOL) and **Refresh**. Warnings: already synced, parent or child
   already synced.
3. Selective sync.

On accept (`accountsettings.cpp` l.918): mkdir, minimum folder permissions,
the fav link (Linux GTK bookmarks: `setupFavLink`), inherit
`ignoreHiddenFiles`, `addFolder`, blacklist/whitelist, schedule. Syncing is
disabled while the wizard is open.

**Selective sync dialog:** "Deselect remote folders you do not wish to
synchronize." A Name/Size tree with checkboxes, the loading, empty and
error states, and OK/Cancel. The output is the blacklist plus an estimated
size.

### 3.7 Conflicts, filename problems, certificates (MUST)

- **Conflict (one file):**
  - two cards (local and server) with a preview, mtime, size and an "Open"
    link each;
  - the OK button text follows the choice: Keep local / Keep server / Keep
    both / Keep selected.
- **Resolve conflicts (many):**
  - "%1 files in conflict", with "All local versions" and "All server
    versions" master checkboxes;
  - per-row local and server cards;
  - "Resolve conflicts" / "Cancel".
- **Invalid filename:**
  - the forbidden characters, basenames and extensions, taken from the
    server capabilities;
  - a new-name field with live validation (including a PROPFIND that the
    name is free);
  - **Rename file** (MOVE on the server, or a local rename) and **Use
    invalid name**.
- **Case clash:** the existing and clashing cards, a new name, and Rename.
  On Windows and macOS this goes with the case-clash conflict solver
  (§7).
- **Untrusted certificate:**
  - automatically rejected under HSTS;
  - otherwise lists the errors and, per certificate, CN, O, OU, C, the
    SHA-1/256/512 fingerprints, the validity dates and the issuer;
  - **Trust this certificate anyway** stores it per account
    (`CaCertificates`).
- **Ignore editors:**
  - the global one: "Sync hidden files" (`ignoreHiddenFiles` per folder)
    and a pattern table with "Allow deletion" (a `]` prefix);
  - system patterns are read-only;
  - the per-folder variant edits `<folder>/.sync-exclude.lst`.
- **Log browser:** "Enable logging to temporary folder"
  (`logToTemporaryLogDir`), Open folder, Close.

---

## 4. Tray behaviour

### 4.1 Icon states (`UP/src/libsync/theme.cpp` `Theme::syncStateIcon` l.717, `folderOfflineIcon` l.789)

| SyncResult status | Icon |
|---|---|
| Undefined | `state-warning` |
| NotYetStarted, SyncRunning | `state-sync` |
| SyncAbortRequested, Paused | `state-pause` |
| SyncPrepare, Success | `state-ok` |
| Problem (success with conflicts) | `state-warning` |
| Error, SetupError | `state-error` |
| disconnected, signed out | `state-offline` |

`state-info` exists but is never used. At startup the icon is offline
until the account is checked (`owncloudgui.cpp` l.106).

Flavours (`Theme::themeIcon(name, sysTray=true)`, l.292):

- `colored/` by default.
- With `monoIcons`: `white/` when the tray is dark, else `black/`.
  - Linux: `Utility::hasDarkSystray()` is always true
    (`utility_unix.cpp` l.141), so mono means white.
  - Windows: the
    `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\SystemUsesLightTheme`
    value decides (`utility_win.cpp` l.303).
  - macOS: `AppleInterfaceStyle`; a mono icon is a **template image**,
    tinted by the menu bar.
- The desktop icon theme wins if it provides `state-ok` and the others
  (`QIcon::fromTheme`).
- SVGs are rasterised at 16 to 256 px.

**For us:** draw **our own six state icons** (our design, not the Nextcloud
logo, as the README promises) in colour plus a monochrome template variant.
Ship them as SVG, rasterise them with `resvg` 0.49 at the sizes each OS
asks for:

- Windows: 16/20/24/32 depending on DPI;
- Linux: SNI `IconPixmap` with several sizes;
- macOS: 18/36 pt template.

Keep `monoIcons` and the same rules for choosing the flavour.

### 4.2 Aggregate state and tooltip (`ownCloudGui::slotComputeOverallSyncStatus`, `owncloudgui.cpp` l.326-532)

1. Any account not connected → offline.
   - Tooltip on Windows (128-character limit): "Disconnected from
     <names>".
   - Elsewhere: "Disconnected from accounts:" then one line per account,
     "Account %1: %2", plus its errors.
2. Every account signed out → offline, "Please sign in".
3. Every folder paused → pause, "Account synchronization is disabled".
4. Otherwise `FolderMan::trayOverallStatus` (`folderman.cpp` l.1790):
   - **One folder:** paused → Paused; Undefined → Error; **Problem shows
     as Success**.
   - **Several folders, by priority:** any error → Error; else any paused →
     Paused; else any running → SyncRunning; else any success → Success;
     else NotYetStarted.
   - Then success + unresolved conflicts → **Problem** (warning icon).
5. Tooltip (`trayTooltipStatusString`, l.1895):
   - Windows: the overall status only.
   - Elsewhere: one line per folder, `"<local path>: <message>"`
     ("Syncing %1 of %2 (%3 left)", "Last sync was successful.", "Sync
     finished with unresolved conflicts.", "Sync is paused.", …).
   - No folders: "There are no sync folders configured."
6. Recompute on a folder state change, an account state change, every
   progress update, and 2 s after `ProgressInfo::Done`.

The computation is pure. Put it in `nc-daemon` (or `nc-gui`) as a function
of the state snapshot and port it with unit tests. Upstream has no direct
test for it, so derive the tests from the code.

### 4.3 Clicks (`ownCloudGui::slotTrayClicked`, `owncloudgui.cpp` l.246-276, `tray/trayactivationpolicy.h`)

- **Left click** (and right click on macOS):
  - no account → the wizard;
  - else, if file-details windows are open → raise them;
  - else, if the popup is open → hide it;
  - else → show the popup (the menu).
- **Double click:** open the current account's local folder.
- **Middle click:** nothing.
- **No tray available** (`main.cpp` l.168-207): wait, retry for up to 30 s on
  XFCE with a "how to install a systray" message, then show Settings and
  retry the tray after 10 s.
- **Wayland:** the popup cannot be positioned, so Settings opens instead.
- **After an account is added**, the popup opens on its own.

### 4.4 Mapping onto tray-icon 0.26.1, per OS

The backends behave differently (from `tray-icon` 0.26.1 `src/lib.rs`
docs):

| | Linux (ksni backend) | Windows | macOS |
|---|---|---|---|
| Thread | own D-Bus worker thread; no GTK, no event loop needed | main thread, winit's message loop | main thread |
| Menu on right click | shown by the SNI host (DBusMenu); the app never sees the click | yes | yes |
| Menu on left click | not possible: the host sends `Activate` (left) and middle-click events | `menu_on_left_click` (default true) | default true |
| Double click event | no | yes (`TrayIconEvent::DoubleClick`) | no |
| Icon rectangle | empty (the protocol has none) | yes | yes |
| Template icon | — | — | `icon_is_template` |

Resulting behaviour:

- **Menu:** the same structure as upstream §3.2, built with `muda`.
  - There is no `aboutToShow` hook on every backend, so rebuild the menu
    when the model changes, throttled to at most once per second.
  - On ksni the menu is a snapshot: call `set_menu` again after a rebuild.
  - Recent activity (5) and the notifications come from the engine and OCS
    models, so the menu stays current without a pre-show fetch.
- **Linux:**
  - right click shows the menu (as upstream: the context menu equals the
    popup);
  - left click (`Activate`) opens the main window on the Activity page;
  - this also covers Wayland, where upstream falls back to Settings.
- **Windows:**
  - left and right click show the full menu;
  - double click opens the local folder;
  - upstream's short classic right-click menu (Open, Pause/Resume,
    Settings, Help, Exit) is a minor divergence. We can reproduce it only
    by switching menus with `set_menu` on click, which is racy. v1 keeps
    one menu and documents the difference.
- **macOS:** a template icon, and the menu on any click, as upstream.
- **No tray host:**
  - On Linux there may be no `StatusNotifierWatcher` (bare X11 window
    managers, GNOME without the AppIndicator extension). `ksni` 0.3.7
    reports this through its `Tray::watcher_offline` /
    `watcher_online` callbacks (`ksni-0.3.7/src/service.rs:95,145`), but
    tray-icon's ksni wrapper does not pass them on. So either check for
    `org.kde.StatusNotifierWatcher` on the session bus ourselves (zbus,
    already a dependency) or use `ksni` directly on Linux.
  - We then do what upstream does: keep the main window visible, show a
    one-time hint (with the GNOME extension), and pick the tray up again
    when a watcher appears.
- **Notification-area quirk (Windows 11):** new icons go to the overflow.
  Nothing to do in code, but the tests must click the overflow chevron
  (§9.2).

---

## 5. Desktop notifications

### 5.1 Events (upstream)

Everything goes through `Logger::postGuiLog` → `Systray::showMessage`
(`systray.cpp` l.1080).

1. **After each sync** (`Folder::createGuiLog`, `folder.cpp` l.456-569):
   **only errors and conflicts** are posted ("Sync Activity"; "%1 has a
   sync conflict…", "%1 and %n other file(s) could not be synced…"). No
   setting turns them off.
2. A new big folder or external storage was discovered ("Please go in the
   settings to select it"). Depends on `newBigFolderSizeLimit` and
   `confirmExternalStorage`.
3. An existing folder grew over the limit (`notifyExistingFoldersOverLimit`,
   mentions `stopSyncingExistingFoldersOverLimit`).
4. A previously excluded item was re-created. The folder watcher is
   unreliable ("Changes in synchronized folders could not be tracked
   reliably").
5. **Server notifications** (OCS `ocs/v2.php/apps/notifications/api/v2/notifications`):
   - **Polling:** every `notificationRefreshInterval` (default and minimum
     60 s, group `[Nextcloud]`), with a 15 s anti-spam window.
   - **Push:** polling stops while the notify_push websocket is up and
     resumes if it drops.
   - **Gating:** shown only if `optionalServerNotifications` is on, the
     account allows desktop notifications, the id was not shown already
     (the set is cleared one hour after the last notification), and the
     server's `shouldNotify` is not false.
   - **Text:** one notification shows its subject (plus the account name
     when there are several accounts); several show "%n notification(s)".
6. Talk chat (`showChatNotifications`) and calls (`showCallNotifications`,
   a QML popup with a ringtone, not an OS toast): LATER.
7. Quota warnings at 80/90/95 % (`showQuotaWarningNotifications`).
8. Unsupported server version, terms of service.
9. Updater, edit locally, URI errors: LATER.

Most server notifications are also added to the Activities list and to the
tray "Notifications" section.

Our `nc-daemon` already has notify_push (`Push` events). It does not have
the OCS notifications or activity clients yet: they are new GUI-side work
in `nc-dav` (new jobs) and `nc-gui` (models).

### 5.2 Mechanism per OS

| OS | Upstream | Ours |
|---|---|---|
| Linux | D-Bus `org.freedesktop.Notifications.Notify`, app name, icon, hint `desktop-entry`, timeout -1, no actions (`USE_FDO_NOTIFICATIONS`) | `notify-rust` 4.18.2 (zbus, pure Rust), same hints |
| Windows | `QSystemTrayIcon::showMessage` (a toast on Windows 10+), no actions | `tauri-winrt-notification` 0.8.1. A toast needs an **AppUserModelID** registered by a Start-menu shortcut. The installer, or the first run, must create it, otherwise toasts are attributed to PowerShell or dropped. Fallback: the tray balloon. |
| macOS | `UNUserNotificationCenter` (`systray_mac_usernotifications.mm`): categories GENERAL, UPDATE (download action), TALK_MESSAGE (reply); authorization asked at startup | v1: `notify-rust` (macOS backend `mac-notification-sys`, `NSUserNotification`, deprecated but working). LATER: `objc2-user-notifications` 0.3.2 in `nc-platform`, which needs a signed `.app` bundle with a bundle id. |

`user-notify` 0.4.2 covers all three OS, but it is LGPL-3.0. Combined with
our GPL-2.0-or-later code, the binary would effectively be GPL-3.0. Avoid it
unless that is acceptable.

---

## 6. Linux-only code in our crates, and how to port it

### 6.1 The big picture

1. **Most "Unix" code already compiles on macOS.** `std::os::unix`,
   `rustix` fs/process, `tokio::net::UnixListener`, `tokio::signal::unix`,
   `nix` `user` and file modes all exist there.
   - **Linux-only:** inotify, `/proc`, `sd-notify`, systemd credentials,
     the XDG paths and the Secret Service.
   - **Windows** needs a split almost everywhere.
2. **`unsafe_code = "deny"` cannot cover the Win32 calls** that have no safe
   wrapper: `SetFileAttributesW`, deny-ACE DACLs, `ReadDirectoryChangesW`,
   `GetVolumeInformationW`, `RmGetList`. Isolate them in `nc-platform`.
3. **`trash` with `default-features = false` does not compile on Windows.**
   `trash-5.2.9/src/windows.rs:321-323` raises a `compile_error!` unless
   `coinit_multithreaded` or `coinit_apartmentthreaded` is enabled. Enable
   `coinit_multithreaded`: the trash calls run on the engine thread, not on
   winit's STA main thread.
4. **There is already a macOS break in an unmodified build.**
   `settings::LockFile` decides that a lock is alive by testing
   `/proc/<pid>` (`nc-daemon/src/settings.rs:976`) and reads the hostname
   from `/proc/sys/kernel/hostname` (`:940`). On macOS every lock therefore
   looks stale.
5. **The macOS journal key needs NFC.** Upstream normalizes the path to NFC
   before `c_jhash64` on macOS (`UP/src/common/syncjournaldb.cpp:1014-1018`).
   Ours does not (`nc-journal/src/journal/mod.rs:192-197`). Without it,
   takeover and the round trip with the official client break on macOS.

### 6.2 Inventory

Paths are relative to `crates/`. The last column is the approach we take.

#### Folder watcher

| Ours | Upstream Windows | Upstream macOS | Approach |
|---|---|---|---|
| `nc-daemon/src/folder_watcher/linux.rs` (inotify port: mask :34, `EINVAL` buffer doubling :353-366, overflow → `LostChanges` :375-377, `ENOMEM`/`ENOSPC` → unreliable :298-302); `folder_watcher.rs:61-63,416-418` (`Inotify` + `AsyncFd`); `daemon.rs:41-77` `inotify_watcher_factory()` | `UP/src/gui/folderwatcher_win.cpp`: one recursive `ReadDirectoryChangesW` on `CreateFileW(longWinPath, FILE_LIST_DIRECTORY, share RWD, BACKUP_SEMANTICS\|OVERLAPPED)` (:28-40); mask FILE_NAME\|DIR_NAME\|LAST_WRITE\|ATTRIBUTES (:63-68); buffer 40 KiB doubled to 64 KiB (:174-182); `ERROR_NOTIFY_ENUM_DIR` → `lostChanges` (:73,103-105) | `UP/src/gui/folderwatcher_mac.cpp`: `FSEventStreamCreate`, latency 0, `UseCFTypes\|FileEvents\|IgnoreSelf` (:101-107); `MustScanSubDirs\|KernelDropped\|UserDropped` → `notifyAll()` (:50-58); only Created/Removed/InodeMetaMod/Renamed/Modified/Cloned (:62-67); paths to NFC (:79) | A `WatcherBackend` trait. Ports on `windows-sys` 0.61.2 and `objc2-core-services` 0.3.2 / `fsevent-sys` 5.2.0 in `nc-platform`; the backends post to the existing channel from a thread. Rename the factory to `default_watcher_factory()`. `notify` 8.2.1 is not faithful on Windows (16 KiB buffer at `notify-8.2.1/src/windows.rs:40`, unwatches on error at :356-366). On macOS it is acceptable as a fallback but lacks `IgnoreSelf`. |
| `folder_watcher.rs:156-161` `file_names_equal` (case-sensitive) | `UP/src/common/utility.cpp:249-259` case-insensitive when `fsCasePreserving()` | same | Add the case-insensitive branch. Compare `\\?\` paths after `dunce` 1.0.5 `simplified`. |

#### Credentials

| Ours | Upstream Windows | Upstream macOS | Approach |
|---|---|---|---|
| `Cargo.toml` (only `zbus-secret-service-keyring-store`); `nc-daemon/src/credentials.rs:200-206` `KeyringStore::secret_service()` (callers `startup.rs:132,250-251`, `manage.rs:75`, `ncsync/src/config_cmds.rs:257`) | QtKeychain 0.15.0 (pinned by `nextcloud/craft-blueprints-kde@stable-34.0`), Credential Manager `CRED_TYPE_GENERIC`, `CRED_PERSIST_ENTERPRISE` | QtKeychain `keychain_apple.mm`: generic password, `kSecAttrService` = service, `kSecAttrAccount` = key, login keychain | Per-target dependencies: `windows-native-keyring-store` 1.1.0, `apple-native-keyring-store` 1.0.2 (feature `keychain`, not `protected`), the Secret Service store on Linux/BSD. `KeyringStore::native()`. |
| `credentials.rs:46,110-132` our own entries (service `ncsyncd`, user = `<user>:<url>/:<id>`) | — | — | Fine on every OS. The Windows store's target becomes `user.service`. App passwords are far below the 2560-byte blob limit. |
| `credentials.rs:543-592` `official_client_app_password()` (takeover: Secret Service search `user=<key>`, `server=Nextcloud`, base64 when `type=base64`) | Target name **`Nextcloud_<key>`**: upstream adds the app name because QtKeychain ≤ 0.16 ignores the service (`UP/src/libsync/creds/abstractcredentials.cpp:49-59`). The blob is **UTF-8**. QtKeychain ≥ 0.17 names it `<key>@<service>`. | service `Nextcloud`, account `<key>`, raw bytes; reading it from another binary triggers a keychain access prompt (item ACL) | Replace the attribute search with `official_item(key) -> Option<Vec<u8>>` per OS. On Windows, build with target `Nextcloud_<key>` (then `Nextcloud_<key>@Nextcloud`) and read with **`get_secret()`**: `get_password()` decodes UTF-16 and fails on QtKeychain's UTF-8. Keep the three key candidates (`credentials.rs:564-571`). |
| Not ported: `UP/src/libsync/creds/keychainchunk.cpp` (2048-byte chunks, at most 10, keys `<key>`, `<key>.1`, …) | used only for client-certificate PEMs and the proxy password, **not for the app password** | no chunking | Not needed for app-password takeover. Needed if client certificates are ported or the official proxy password is read. |
| `credentials.rs:10-25` (systemd credentials), `:526-541` (0o700/0o600 password file) | — | — | systemd part: `cfg(target_os = "linux")`. Modes: `cfg(unix)`. Windows files under `%LOCALAPPDATA%` inherit the profile ACL. |

#### Locations

| Ours | Upstream Windows | Upstream macOS | Approach |
|---|---|---|---|
| `nc-daemon/src/config_file.rs:288-311` (`$XDG_CONFIG_HOME/ncsyncd/ncsyncd.cfg`, `$XDG_STATE_HOME/ncsyncd`) | upstream config: `%APPDATA%\Nextcloud\` (roaming, `QStandardPaths::AppDataLocation`), migrated from `%LOCALAPPDATA%\Nextcloud` (`UP/src/libsync/configfile.cpp:350-375`) | `~/Library/Preferences/Nextcloud/` | `dirs` 7.0.0: `config_dir()`; state in `data_local_dir()` (`state_dir()` is Linux-only). Do not use `directories::ProjectDirs`: on Windows it appends `\config`. |
| `config_file.rs:345-349` `official_client_config_file()` | `%APPDATA%\Nextcloud\nextcloud.cfg` = `dirs::preference_dir()/Nextcloud` | **The v34 macOS app is sandboxed** (`UP/admin/osx/macosx.entitlements.cmake`), so first try `~/Library/Containers/com.nextcloud.desktopclient/Data/Library/Preferences/Nextcloud/nextcloud.cfg`, then `dirs::preference_dir()/Nextcloud/nextcloud.cfg` (older builds) | The sandboxed folders may hold security-scoped bookmarks (`UP/src/common/macsandboxpersistentaccess.mm`); a raw group copy keeps them. |
| `config_file.rs:271-275,316-337` system instance `/var/lib/ncsyncd/<user>` | — | — | `cfg(target_os = "linux")`. A Windows service (`windows-service` 0.8.1) is LATER. |
| `account_setup.rs:342` `$HOME`; `ncsync/src/netrc.rs:26` | `QDir::homePath()`; netrc stays `home/.netrc` (`UP/src/cmd/netrcparser.cpp:28`) | `$HOME` | `dirs::home_dir()` |
| `ncsync/src/main.rs:297-312` `system_exclude_file()` | `applicationDirPath()/sync-exclude.lst` (`configfile.cpp:404-406`) | `../Resources/sync-exclude.lst` (:431-434) | next to `current_exe()`, or in the bundle's `Resources` |
| `nc-daemon/src/settings.rs:886-896` (new config 0o600, atomic rename) | inherits the ACL; `rename` may hit a sharing violation | as Unix | `cfg(unix)` modes; retry the rename on a sharing violation. **To verify:** does QSettings `IniFormat` write CRLF on Windows? If so, `ini-preserve` must keep CRLF and the byte-identical test needs a Windows variant. |
| `settings.rs:913-993` `LockFile` (`/proc` liveness and hostname) | QLockFile keeps the handle open | `/proc` is missing: **broken today** | hostname: `rustix::system::uname()` (Unix) or `GetComputerNameExW`; liveness: `rustix::process::test_kill_process` (Unix) or `OpenProcess` + `GetExitCodeProcess` (or `sysinfo` 0.40.1) |

#### IPC, signals, service

| Ours | Upstream | Approach |
|---|---|---|
| `nc-daemon/src/control.rs:84-95` socket path (`$XDG_RUNTIME_DIR`, `/tmp/ncsyncd-<uid>`), `:246-293` `serve()` (`UnixListener`, 0o700/0o600), `:304-316` `request()` (std `UnixStream`) | socket API: Windows `\\.\pipe\<exe>-<USERNAME>`, Linux `RuntimeLocation/<app>/socket`, macOS XPC (`UP/src/gui/socketapi/socketapi.cpp:251-312`) | `interprocess` 2.4.7 (`tokio`): Unix sockets / named pipes; an owner-only security descriptor on Windows; `FILE_FLAG_FIRST_PIPE_INSTANCE`. Hash-based names (§2.3). |
| `daemon.rs:84-104` `tokio::signal::unix` (TERM, INT, HUP) | — | Unix unchanged. Windows: `tokio::signal::windows::{ctrl_c, ctrl_break, ctrl_close, ctrl_shutdown, ctrl_logoff}` → `Shutdown`; reload only through the control request. |
| `daemon.rs:79-82,174-178,215-248` `sd_notify`; `contrib/systemd/*` | `UP/systemd/nextcloud-desktop.service.in` | `sd-notify` becomes a `cfg(target_os = "linux")` dependency, behind ncsyncd hooks. Add `contrib/launchd/*.plist` (LaunchAgent, `KeepAlive`). Windows: per-user autostart, or a service later. |
| Single instance: not ported | `KDSingleApplication` (QLocalServer: named pipe or Unix socket), macOS `SingleInstanceManager` at `homePath()/si` (`UP/src/gui/macOS/singleinstancemanager_mac.mm:19-75`) | the guard lock + control endpoint (§2.3) |
| Autostart: not ported | Windows `HKCU\…\CurrentVersion\Run` (`UP/src/common/utility_win.cpp:33,286-296`); macOS `SMAppService mainAppService` (`utility_mac.mm:100-135`); Linux XDG autostart `.desktop` (`utility_unix.cpp:60-135`) | §8.4 |

#### Detecting the official client

| Ours | Windows / macOS | Approach |
|---|---|---|
| `nc-daemon/src/takeover.rs:365-427` (`/proc/*/comm`, `exe`, `cmdline`, uid via `MetadataExt::uid`) | Windows: `nextcloud.exe` with the same SID; macOS: `Nextcloud.app/Contents/MacOS/Nextcloud` with the same uid. More precise: probe the official single-instance endpoint (macOS `~/Library/Containers/com.nextcloud.desktopclient/Data/si`; the KDSingleApplication name on Windows/Linux, to be checked). | `sysinfo` 0.40.1 (`default-features = false`, `system`, `user`), or keep `/proc` behind a cfg. |
| `nc-daemon/src/instance.rs:28-117` (root hands the files to a user) | n/a | `cfg(unix)` |

#### Local file system (`nc-sync/src/filesystem.rs`, "Port of the Unix parts")

Paths stay `/`-separated strings, as upstream's `QString`. Convert to native
form only at the Win32 boundary, with a port of `FileSystem::longWinPath()`
(`\\?\` + backslashes, `UP/src/common/filesystembase.cpp:35-41,800-821`).

| Ours | Upstream Windows | Upstream macOS | Approach |
|---|---|---|---|
| :46-69 `csync_vio_local_stat` (lstat, `ino`, `mtime`, socket → SoftLink, world-writable → invalid) | `UP/src/csync/vio/csync_vio_local_win.cpp:198-224`: `GetFileInformationByHandle` with `OPEN_REPARSE_POINT`; **inode = `nFileIndexHigh:Low`**; mtime = `ftLastWriteTime` | `UF_HIDDEN` → `is_hidden` (`csync_vio_local_unix.cpp:153-157`) | Split into `filesystem/{unix,windows}.rs`. File index: `file-id` 0.2.3 (check that it does not follow reparse points), or `nc-platform`. The journal `inode` stays `u64`. |
| :87-128 `csync_vio_local_readdir` (byte names, d_type, errno 13/2/20) | `FindFirstFile`; reparse SYMLINK/MOUNT_POINT → SoftLink; DEVICE/OFFLINE → Skip; **HIDDEN → `is_hidden`** (`csync_vio_local_win.cpp:49-180`) | `st_flags & UF_HIDDEN` | `MetadataExt::file_attributes()` (safe) on Windows; `std::os::macos::fs::MetadataExt::st_flags()`. Map errors 5/2/3/267. Names: `OsStr::to_str()`; unpaired surrogates → Skip. **`is_hidden` is set on no OS today.** |
| :151-154 `is_writable` (`access(W_OK)`), `folder_definition.rs:677`, `folder_watcher/linux.rs:168` | `isWritable`/`isReadable` with an NTFS permission lookup (`filesystembase.cpp:492-548`); `Utility::canCreateFileInPath` probe (`folderman.cpp:1996-2001`) | access() | Windows: the read-only attribute + a probe file. |
| :172-207 `get_mod_time` / `set_mod_time` (`utimensat`) | `SetFileTime` with `BACKUP_SEMANTICS` (`UP/src/csync/std/c_time.cpp:50-62`); first makes the item writable to get past deny ACEs (`UP/src/libsync/filesystem.cpp:433-436`) | utimes | `filetime` 0.2.29 (handles directories), plus a permission-restore wrapper on Windows. |
| :232-268 `rename`, `unchecked_rename_replace`, `remove` | `MoveFileExW(longWinPath, COPY_ALLOWED\|WRITE_THROUGH)` keeps trailing dots and spaces (`filesystembase.cpp:202-218`); clear read-only before removing (:601-608) | Qt | Always pass verbatim `\\?\` paths on Windows. |
| :271-358 read-only files and folders (mode bits, umask) | files: `FILE_ATTRIBUTE_READONLY` + ACL; **folders: a deny-ACE DACL** (`setAclPermission`, `filesystembase.cpp:823-977`); `isFolderReadOnly` reads the DACL (:715-730) | + `setFolderMinimumPermissions` (`filesystembase.cpp:151-159`, noted at `nc-daemon/src/folder.rs:1482`) | `windows` 0.62.2 (`Win32_Security_Authorization`) in `nc-platform`; `windows-acl` is unmaintained. |
| `nc-sync/src/propagator/download.rs:1058-1073` (copy the mode, `chown` the group) | copy the ACL (`propagatedownload.cpp:1184-1192`) | as Unix | `cfg(unix)` + a Windows ACL branch |
| :405-408 `is_file_locked` → `false` (callers `download.rs:1134`, `propagator/mod.rs:677,754`, `discovery.rs:3799`) | `CreateFileW` with a share mode per `LockMode` + `LockFile`; errors 32/33 → locked (`filesystembase.cpp:676-766`) | false | `OpenOptionsExt::share_mode()` (safe) + `File::try_lock` / `fs4`. Port the `LockMode` enum. Add the Windows `LockWatcher` (`UP/src/gui/lockwatcher.cpp`): it re-schedules a sync once a locked file is released (`folder_man.rs:8-9` notes it is missing). |
| :410-411 `set_file_hidden` no-op (callers `download.rs:643,1141`, `folder_watcher.rs:363`, `nc-journal/src/journal/mod.rs:1015`) | `SetFileAttributesW(HIDDEN)`; the journal hides `.db`, `-wal`, `-shm` | no-op | `nc-platform` |
| :494-500 `free_disk_space` (`statvfs`) | `GetDiskFreeSpaceExW` | statvfs | `fs4::available_space` |
| `nc-journal/src/journal/mod.rs:496-504` journal mode (`TODO(windows)`) | FAT → DELETE (`syncjournaldb.cpp:86-96`, `GetVolumeInformationW`) | `/Volumes/` → DELETE (done) | `nc-platform` |
| :147-158 symlinks, `.lnk` | `isSymLink`, `isJunction`, `.lnk` size (`filesystem.cpp:486-497`) | — | `FileType::is_symlink()` also covers junctions. |
| Missing: `openAndSeekFileSharedRead` (`filesystembase.cpp:239-311`) | read with share RWD while others write | — | `share_mode(READ\|WRITE\|DELETE)` for every upload/checksum open in `nc-sync/src/propagator/upload.rs` and `nc-journal` checksums. |
| `nc-sync/src/propagator/local.rs:70` `trash::delete` | Recycle Bin (`QFile::moveToTrash`) | Finder trash | `trash` 5.2.9 + `coinit_multithreaded` |
| byte names, `from_utf8_lossy` | UTF-16 | NFD on HFS+; upstream normalizes FSEvents and readdir names to NFC | `unicode-normalization` 0.1.26 at the same places |

#### Case and name rules

| Ours | Upstream | To do |
|---|---|---|
| `nc-journal/src/utility.rs:12-80` `fs_case_preserving()` (env `OWNCLOUD_TEST_CASE_PRESERVING`, default Windows or macOS) and its callers; `nc-sync/src/sync_file_status_tracker.rs:39` | same | already right at compile time; the suite passes with `OWNCLOUD_TEST_CASE_PRESERVING=1` |
| `nc-sync/src/propagator/mod.rs:589-611` `local_file_name_clash`: only the Linux test branch | macOS: `canonicalFilePath().normalized(NFC)`; Windows: `FindFirstFileW` real-name compare (`UP/src/libsync/owncloudpropagator.cpp:802-845`) | port both branches |
| `hasCaseClashAccessibilityProblem` returns false (`propagator/bulk.rs:325`) | Windows only (`owncloudpropagator.cpp:848-880`) | port |
| `nc-journal/src/journal/mod.rs:192-197` `get_phash` | NFC on macOS | **required before macOS ships** |
| `nc-sync/src/engine.rs:640-654` Windows name compatibility only from capabilities | always on Windows (`UP/src/libsync/syncengine.cpp:685-688`) | add `cfg!(windows)` |
| `nc-sync/src/discovery.rs:1478-1500,1559,3661-3670` trailing spaces, `maybe_rename_for_windows_compatibility` | Windows condition differs (`UP/src/libsync/discovery.cpp:283-288`) | add the Windows branch |
| `nc-journal/src/exclude/mod.rs:175-186,275-295,577` reserved words, invalid characters, `wildcards_match_slash` | `UP/src/csync/csync_exclude.cpp:162` | already under `cfg(windows)` |
| `discovery.rs:3029` `queryEditorsKeepingFileBusy` not ported | Restart Manager `RmGetList` (`UP/src/common/utility_win.cpp:37-100`); empty elsewhere | `windows` `Win32_System_RestartManager` in `nc-platform` |

#### Paths and separators

There are 15 places that treat `starts_with('/')` as "is absolute", and many
`rfind('/')`. These break on `C:/…`:

- `nc-sync/src/touched_files.rs:104`, `propagator/remote.rs:341`,
  `propagator/upload.rs:391`;
- `nc-daemon/src/folder.rs:66,1647`, `folder_definition.rs:173,183,612`,
  `manage.rs:701`, `takeover.rs:149`;
- `nc-sync/src/filesystem.rs:361-367`.

Upstream writes `localPath` with forward slashes even on Windows
(`C:/Users/x/Nextcloud/` in `nextcloud.cfg`). Keep that, and add:

- `is_absolute_slash_path()` (accepts `X:/` and `//server/share`);
- `long_win_path()`;
- `isPathWindowsDrivePartitionRoot` (`UP/src/common/utility.cpp:588-607`);
- `dunce` for display.

#### Other

- `nc-dav/src/account.rs:461-467`: the User-Agent hard-codes `(Linux)`.
  Upstream uses `Windows` or `Macintosh` (`UP/src/common/utility.cpp:131-152`).
- `nc-daemon/src/connection_validator.rs:17`: macOS local-network
  permission (`LocalNetworkPermission`, macOS 15+) is not ported.
- Already portable: `rpassword`, `reqwest` `system-proxy` (Windows
  registry, macOS SystemConfiguration), `rustls-platform-verifier`
  (OS stores), `rusqlite` bundled.
- `aws-lc-rs` (through `tokio-rustls` and `rcgen`) needs CMake and NASM in
  Windows MSVC CI.
- Unix-only test helpers (`nc-testutils/src/folder.rs:483-494`, the 0o700
  restores, `/dev/zero` in `nc-journal/src/checksums/tests.rs:152`) get
  cfg gates as upstream does (`#ifndef Q_OS_WIN testPropagatePermissions`).

#### CI and release

- `.github/workflows/ci.yml` builds Linux only (its comment says so).
- Add:
  - `windows-2025` (`x86_64-pc-windows-msvc`), and `windows-11-arm` later;
  - `macos-15` (`aarch64-apple-darwin`) plus `x86_64-apple-darwin`.
- Run `clippy --all-targets --all-features -D warnings` **on each OS**:
  cfg-gated code is invisible to the Linux clippy.
- Packaging:
  - Windows: a `.zip` (and an installer later), signed through the
    existing `signing` environment (Certum);
  - macOS: a signed and notarized `.app` / `.dmg` for the GUI, and a
    `.tar.gz` for the CLI;
  - Linux: as today, plus the GUI tarball, a `.desktop` file and an icon.
- `README.md:357-366` "Linux only" must change.

---

## 7. Upstream tests that become applicable

From `docs/test-parity.md` (upstream files under `UP/test/`).

### 7.1 With Windows

| Upstream file | Function(s) | Today | With Windows |
|---|---|---|---|
| `testutility.cpp` | `testIsPathWindowsDrivePartitionRoot` | n/a | port the function + test |
| `testutility.cpp` | `testFileNamesEqual` | n/a ("GUI only") | **applicable already**: our folder watcher calls it; the case-insensitive branch matters on Windows and macOS |
| `testlongpath.cpp` | `check_long_win_path` (+ the Windows part of `initTestCase`) | n/a | port `pathtoUNC` + test |
| `testfilesystem.cpp` | `testRenameFileWithTrailingPeriod`, `testAclWithManyDeniedAces` | n/a | applicable (verbatim paths; the `setAclPermission` port) |
| `testfilesystem.cpp` | `testSetFileReadOnlyLongPath` (Windows block), `initTestCase` | ported (Unix) | add the Windows branch |
| `testlockedfiles.cpp` | `testLockDetectionUsesRealFileSystemCheck`, `testDirectoryLockChecks`, `testLockedFilePropagation`, `testPartialRecursiveRemoteRemovalNormalisesJournalPaths`, `testPartialRecursiveRemoteRemovalDoesNotDeleteRemoteFileOnNextSync` | n/a | applicable (`isFileLocked`) |
| `testlockedfiles.cpp` | `testBasicLockFileWatcher` | n/a | applicable with the `LockWatcher` port |
| `testlockedfiles.cpp` | `testLocalDirectoryDiscoveryReturnsAllEntries` (Windows block) | adapted | add the Windows part |
| `testlocaldiscovery.cpp` | the four `testCreateFileWithTrailingSpaces_*` | non-Windows branch | port the `Q_OS_WINDOWS` branches (:445,475,546,632) |
| `testexcludedfiles.cpp` | `check_csync_excluded`, `check_csync_excluded_traversal` | `cfg(windows)` blocks ported | they finally run |
| `testsyncengine.cpp` | `testInvalidFilenameRegex`, `testPropagatePermissions` | ported | gate the `#ifndef Q_OS_WIN` parts |
| `testsyncengine.cpp` | 8 `testServer_caseClash_*` / `testServer_subFolderCaseClash_*` | Linux branch | upstream expects case-clash conflicts on Windows **and** macOS: needs `UP/src/libsync/caseclashconflictsolver.cpp` (not ported) |
| `testdownload.cpp` | `testMoveFailsInAConflict` | adapted | QSKIP on Windows, as upstream |
| `testfolderman.cpp` | `testCheckPathValidityForNewFolder` | adapted | add the Windows and drive-letter rows (:272, :381) |
| `testfolder.cpp` | `test_sidebarDisplayName` | n/a | applicable with the Explorer navigation pane (LATER) |
| `testpermissions.cpp` | `setFileHidden` helper | — | needs `set_file_hidden` |
| `testnextcloudcmdprovisioning.cpp` | 5 provisioning tests | exit code 255 | upstream expects **-1** on Windows |
| `testfolderwatcher.cpp` | the whole file (15 functions; Windows helper variants :19-66) | inotify | run against the Windows and macOS backends; `CHECK_WATCH_COUNT` stays Linux-only |

### 7.2 With macOS

| Upstream file | Function | Today | With macOS |
|---|---|---|---|
| `testconnectionvalidator.cpp` | `localNetworkPermissionFailureReplacesTimeout` | n/a | applicable once `LocalNetworkPermission` is ported |
| `testfolderwatcher.cpp` | `testDetectLockFilesExternally` | ported | QSKIP on macOS, as upstream |
| `testsyncmove.cpp` | `testMovedWithError` | adapted | QSKIP "not reliable on macOS" |
| `testnextcloudcmdprovisioning.cpp` | `testNoArgsShowsHelp` | exit 0 | upstream expects 255 on macOS |
| `testsyncengine.cpp` | the case-clash tests | Linux branch | as on Windows |
| (derived) | NFC `get_phash` | — | add a test |
| `testutility.cpp` | `testLaunchOnStartup` | n/a (GUI) | becomes a GUI test (QSKIP on macOS upstream) |

### 7.3 Stay n/a

- macOS File Provider (`testfolderman.cpp` :736-738, `testaccountmanager.cpp`
  `testAccountFromUserId_*`, `UP/test/macOS/*`);
- CfApi VFS (`testsynccfapi.cpp`, `testcfapishellextensionsipc.cpp`, the
  `Vfs::WindowsCfApi` row of `testMovedWithError`, `testsyncvirtualfiles.cpp`);
- Linux-only tests (`testinotifywatcher.cpp`,
  `testDeletingComputeChecksumBeforeCalculationCompletionDoesNotCrash`);
- GUI-only `testSetupFavLink`. It becomes portable once the folder wizard
  creates GTK bookmarks.

Takeover on macOS must still refuse the official client's File Provider
accounts, as it already refuses VFS folders.

---

## 8. Crate choices for the GUI

### 8.1 egui stack (as in PinReady and SSR)

- `eframe = { version = "0.37", default-features = false, features =
  ["accesskit", "default_fonts", "glow", "wayland", "x11", "persistence"] }`.
- `egui = { version = "0.37", features = ["monochrome_emoji_fonts"] }`.
- An `inspection` feature that maps to `eframe/inspection`, development
  only (for `egui_mcp`).
- `egui_extras` 0.37 (`svg`, `image`) for avatars, server app icons and
  thumbnails; `resvg` 0.49 for the tray icons.
- **Theme:** a `theme.rs` module with the PinReady/SSR palette approach.
  `egui::ThemePreference::{Light, Dark, System}`. eframe follows the system
  theme through winit, and on Linux through the XDG portal setting.
  Saved in our own config group, next to the zoom.
- **Fonts and icons:** the embedded fonts have no emoji
  (`monochrome_emoji_fonts` only adds a monochrome set). Paint the icons in
  an `icons.rs` module, as clap-adn does. The safe text glyphs are
  ✓ ✂ ▶ ← → ↑ ↗ ⚠ ◐ ☀ −.
- **i18n:** `rust-i18n` 4.2 with TOML `locales/` and `sys-locale` 0.3.2
  for detection, as PinReady does (`src/i18n.rs`), plus a language
  selector. The official client ships 64 Qt Linguist files
  (`UP/translations/client_*.ts`, GPL like us). A small converter could
  seed our TOML files from them for every string we keep word for word
  (decision D6).
- **Persistence:** eframe `persistence` for the window geometry. Settings
  that upstream owns go to `nextcloud.cfg` keys, never to eframe storage.

### 8.2 Tray icon: options

| Crate | Linux | Windows | macOS | Event loop integration | Verdict |
|---|---|---|---|---|---|
| **`tray-icon` 0.26.1** (tauri) + `muda` 0.21.2 | AppIndicator (GTK 3 + libappindicator, needs a GTK loop on the thread) **or `ksni` feature (SNI over D-Bus on a worker thread, no GTK)** | Win32 `Shell_NotifyIcon`; needs a message loop on its thread (winit's) | NSStatusItem; main thread | events through `TrayIconEvent::set_event_handler` / `MenuEvent::set_event_handler` → forward to a channel + `ctx.request_repaint()`; handled in `App::logic()` | **Recommended**, with `default-features = false, features = ["ksni"]` |
| `ksni` 0.3.7 alone | SNI, tokio or async-io | — | — | own thread | already wrapped by tray-icon; use it directly only if tray-icon's snapshot menus fall short |
| tray-icon default (libappindicator) | GTK 3 + libappindicator at runtime and build time; a GTK main loop must run on the creating thread. With winit that means a second thread running `gtk::main()`. | — | — | awkward | rejected: a GTK dependency and an extra event loop just for the tray |
| `tray-item`, `systray` | older, GTK-based on Linux | — | — | — | rejected |

Why ksni: StatusNotifierItem is what KDE Plasma, GNOME (with the
AppIndicator extension, on by default in Ubuntu), XFCE (with its SNI
plugin), Cinnamon, LXQt, Sway/waybar and Hyprland show. It works the same
on X11 and Wayland, because the tray is not a window. Qt6, which upstream
uses, also uses SNI first.

Integration rule: create the `TrayIcon` inside the eframe `AppCreator`
closure (main thread, after the event loop exists). On macOS it must be
created after the app has finished launching, which this satisfies. Keep
the handle in the `App`.

### 8.3 Single instance

There is no separate crate. Use the guard lock and the control endpoint
from §2.3:

- a second launch connects and sends `Show` (later also `OpenUri(nc://…)`
  and command-line arguments, like KDSingleApplication's message);
- if the connection fails while the lock is held, report the process named
  in the lock file.

`single-instance` 0.3.3 (a named mutex / abstract socket) would add a
second mechanism and cannot carry the message, so it is not needed.

### 8.4 Autostart

Upstream behaviour, per OS:

- **Windows:** `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value
  = app name, data = the quoted exe path plus `--background`.
- **Linux:** `~/.config/autostart/<app id>.desktop` with
  `X-GNOME-Autostart-Delay`.
- **macOS:** `SMAppService.mainAppService` register/unregister.

Recommendation: our own module of about 100 lines.

- Windows through `windows-registry` 0.100 (safe API).
- Linux by writing the `.desktop` file.
- macOS v1: a LaunchAgent plist in `~/Library/LaunchAgents` (works for an
  unsandboxed app and for a bare binary). LATER: `SMAppService` through
  `objc2-service-management` 0.3.2 once the app is a signed bundle.
- Detect a system-wide autostart and disable the switch, as upstream does.

`auto-launch` 0.6.0 does the same three things, but with its own
naming (and AppleScript login items on macOS). It is acceptable if we do not
care about matching upstream's entry format.

The wizard turns autostart on when it finishes, as upstream does.

### 8.5 Notifications

See §5.2: `notify-rust` 4.18.2 (Linux, and macOS for v1),
`tauri-winrt-notification` 0.8.1 (Windows),
`objc2-user-notifications` 0.3.2 (macOS, LATER).

### 8.6 Browser, file manager, dialogs, clipboard

- `opener` 0.9 with the `reveal` feature:
  - `open_browser` for Login Flow, apps, help and terms of service;
  - `open` for local files and folders;
  - `reveal` for "show in folder" with the file selected (upstream
    `openfilemanager.cpp`: Explorer `/select`, `open -R`, D-Bus
    `org.freedesktop.FileManager1.ShowItems`).
- `rfd` 0.17 with `xdg-portal` + `pollster` (no GTK): folder pickers and
  the PKCS#12 picker.
- The clipboard ("Copy link") through egui's `ctx.copy_text`.

### 8.7 Others

| Need | Crate |
|---|---|
| Debug archive | `zip` 9.2.2 |
| GUI log files | own `log::Log` backend (upstream format) or `flexi_logger` 0.31.10 |
| Light/dark taskbar (Windows mono icon choice) | `windows-registry` read of `SystemUsesLightTheme` |
| Process probe (official client) | `sysinfo` 0.40.1 |
| UI tests | `egui_kittest` 0.37 |

---

## 9. Phased implementation plan

Each milestone ends with something that can be tested on its own. Engine
work (P1 to P3) comes before the GUI, because the GUI's first useful run
needs the engine on its OS.

### P0. Engine API for an embedding GUI (Linux, no GUI)

- Drain `take_requests()` (and fix ncsyncd's silent drop).
- Add the `UiEvent` broadcast.
- Move `sd_notify`, the watchdog and the signals into ncsyncd hooks.
- Add the guard lock (before setup) and hash-based endpoint names.
- Extend `control::Request` with every GUI operation and add an event
  subscription carrying `UiEvent`, so the GUI can drive ncsyncd remotely
  (D3). Define the GUI's engine interface once, with an in-process
  implementation and a remote one over the control endpoint.
- Port the aggregate tray-state and tooltip computation (§4.2) as a pure
  function.

**Done (2026-11-02):** `nc_daemon::ui_event` (the `UiEvent` broadcast and
the snapshots), `FolderMan::take_requests()` drained into
`DaemonHooks::manager_request`, ncsyncd's `SystemdHooks`
(`crates/ncsyncd/src/systemd.rs`), `nc_daemon::guard`,
`nc_daemon::operations` and the new `control::Request`s, `nc_daemon::client`
(`EngineClient`, `InProcess`, `Remote`), `nc_daemon::tray_status`, and
`ncsync events`. Left for later phases:

- The prompts that carry a reply (`NewBigFolder`, `AboutToRemoveAllFiles`,
  `SslErrors`, `ProxyAuthRequired`) and an "accept a prompt" request. The
  engine's `aboutToRemoveAllFiles` callback answers synchronously, so a
  dialog needs that callback made asynchronous first (P5, with the dialogs);
  `NewBigFolder`, `ExistingFolderNowBig` and the restored mass deletion are
  sent as notifications. There is no SSL-error or proxy-authentication path
  in the engine yet.
- `QuotaChanged` and `ConflictsChanged` as events of their own: there is no
  quota job yet (P6), and the unresolved-conflicts flag travels in
  `FolderStateChanged`.
- `ProgressInfo` keeps no transfer-rate estimates, so `eta_msecs` is always
  `None` and the tooltip never shows "(%1 left)".
- The control endpoint is still a tokio `UnixListener`; `interprocess` and
  named pipes come with P1.

**Test:**

- unit tests for the aggregate state (one per branch of
  `trayOverallStatus` and `slotComputeOverallSyncStatus`);
- a guard test (two processes on one config: the second is refused; after
  `kill -9` of the first, the lock is free again);
- a remote test: ncsyncd runs, a client subscribes over the endpoint,
  receives the events of a sync and pauses and resumes a folder;
- `tools/itest/run-daemon.sh` stays green.

### P1. `nc-platform` and the safe portable fixes (all OS compile)

- Create `nc-platform`.
- Move to `interprocess` for the control endpoint.
- `dirs` for every location.
- Fix `LockFile` (`/proc` on macOS).
- `trash` `coinit_multithreaded`.
- Per-target keyring stores.
- User-Agent per OS.
- Gate the systemd and `/proc` code with cfgs.
- Add CI jobs on `windows-2025` and `macos-15` that run `cargo build`,
  `clippy -D warnings` and the full ported test suite.

**Test:** the CI matrix is green on the three OS. The Windows-only and
macOS-only tests that need P2 stay marked as pending in
`docs/test-parity.md`.

**Status (2026-11-03): done**, except that the CI result of the new
`portable` job is reported in the commit that added it.

- `nc-platform` is the only crate with `unsafe` (`undocumented_unsafe_blocks`
  is a warning there; `unsafe_code = "deny"` stays everywhere else). It holds
  the pure path helpers (`long_win_path`, `clean_path`, `is_absolute`,
  `to_slash`, `from_native`), host name and process liveness for the lock
  file, the descriptor registration of the Linux watcher, and the Win32 calls.
- The control endpoint is an `interprocess` local socket (`nc_daemon::transport`):
  a Unix socket as before, a named pipe `\\.\pipe\ncsyncd-<USERNAME>-<hash>` on
  Windows. The blocking client side (`ncsync`, `EngineClient::Remote`) keeps
  its read timeout through a reader thread on Windows. The pipe uses the
  default security descriptor (owner-only DACL and `FILE_FLAG_FIRST_PIPE_INSTANCE`
  are not done yet, see "Left" in P2).
- `LockFile` uses `rustix` (`uname`, `test_kill_process`) or Win32.
- Config paths: `dirs` off Linux; the official client's file is looked up in
  `%APPDATA%\Nextcloud` then `%LOCALAPPDATA%\Nextcloud` (Windows) and in the
  sandbox container then `~/Library/Preferences/Nextcloud` (macOS).
- Credentials: `KeyringStore::native()` (Secret Service, Credential Manager,
  login keychain). `SecretStore::official_item` reads QtKeychain's items:
  Windows target `Nextcloud_<key>` then `<key>@Nextcloud` with `get_secret`.
- macOS: NFC `get_phash` (tested on every OS through `get_phash_with`),
  `UF_HIDDEN`. macOS has **no folder watcher** yet (it reports itself
  unreliable, so the folder rescans periodically) and nothing was run on a
  Mac: macOS is experimental (D4), validated by the `macos-15` CI job only.
- `trash` has `coinit_multithreaded`; the User-Agent still says `(Linux)`
  (**left**).
- Not part of the original list but needed: the ported tests that set POSIX
  modes (`testpermissions`, `testPropagatePermissions`, the mode half of
  `testfilesystem`, `testMoveFailsInAConflict`) are `cfg(unix)`, as upstream
  `#ifndef Q_OS_WIN` / `QSKIP`.

### P2. Windows engine parity

In order:

1. `long_win_path`, absolute-path helpers, verbatim paths;
2. readdir and stat (file index, hidden/device/offline attributes);
3. `set_file_hidden`, read-only files;
4. ACL folders;
5. `is_file_locked` + `LockMode`, then `LockWatcher`;
6. shared-read opens;
7. mtime through `filetime`;
8. FAT → DELETE journal mode;
9. the `ReadDirectoryChangesW` watcher;
10. Windows name compatibility and trailing spaces;
11. `local_file_name_clash` and `hasCaseClashAccessibilityProblem`;
12. `caseclashconflictsolver.cpp`;
13. Restart Manager editors;
14. Windows keychain takeover (`Nextcloud_<key>`, `get_secret`);
15. the Ctrl events;
16. CRLF config check.

**Status (2026-11-03).** Items 1, 2, 3 (files), 5 (`is_file_locked` and
`LockMode`), 6 (std opens already share read/write/delete), 7, 8, 9, 10 (the
`should_enforce_windows_file_name_compatibility` switch only), 14 and 15 are
done; the engine's own tests pass in the Windows VM (see the commit message for
the exact count) and a real `ncsync sync` / `ncsyncd` run against the itest
server works from the VM.

**Left:**

- item 4: folder ACLs (`setAclPermission`, `isFolderReadOnly`): folders are
  never made read-only on Windows, `testAclWithManyDeniedAces` is pending;
- item 5: the `LockWatcher` (re-sync once a locked file is released);
- item 10: the Windows branches of the four `testCreateFileWithTrailingSpaces_*`
  tests and `maybe_rename_for_windows_compatibility` (those tests are
  `cfg(not(windows))` for now);
- items 11 and 12: `local_file_name_clash`, `hasCaseClashAccessibilityProblem`
  and `caseclashconflictsolver.cpp` (the 7 case-clash tests are
  `cfg(not(any(windows, macos)))`);
- item 13: Restart Manager editors;
- item 16: whether QSettings writes CRLF on Windows is still unchecked
  (`ini-preserve` keeps whatever the file has);
- the named pipe has the default DACL, no `FILE_FLAG_FIRST_PIPE_INSTANCE`;
  `testfolderwatcher_native.rs` ports 10 of the 15 `testfolderwatcher.cpp`
  functions; `test_upload_while_file_is_changing` is pending on Windows;
- takeover against the official Windows client v34.0.5 (not installed in the
  VM yet).

**Test:**

- port the §7.1 tests, which become ported rows in `docs/test-parity.md`;
- run the ported suite natively in the Windows VM (`winssh 'cargo test
  …'`) and on the CI runner;
- itest against the real server from the VM (§9.2);
- takeover from the official Windows client v34.0.5 installed in the
  disposable VM, then hand-back.

### P3. macOS engine parity

- NFC for `get_phash` and the watcher and readdir names;
- `UF_HIDDEN`;
- the FSEvents watcher;
- `setFolderMinimumPermissions`;
- the sandboxed official config path;
- the Keychain takeover;
- the case-clash solver (shared with P2);
- the launchd plist for ncsyncd.

**Test:** the §7.2 tests on the `macos-15` runner. The takeover and keychain
prompt are not validated on a real Mac (decision D4: macOS is experimental).

### P4. GUI skeleton and tray (three OS)

- The `nc-gui` crate and the GUI binary.
- eframe glow with theme, zoom, i18n and the icons module.
- The engine thread, the guard and single instance (`Show`).
- `tray-icon` with our state icons and tooltip from P0, and the menu of
  §3.2 (accounts with "Local folder", Pause/Resume all, Settings, Quit;
  apps, notifications and activity come in P6).
- Hide to tray.
- No-tray fallback.
- Autostart module.

**Test:**

- `egui_kittest` (window opens, theme switch, language switch);
- **Linux tray headless:** in the Xvfb container, start a session D-Bus
  and a small test `StatusNotifierWatcher` (a zbus helper in
  `tools/guitest`) that records the registered item and reads
  `IconPixmap`, `ToolTip` and the DBusMenu `GetLayout`. Assert the icon
  state and the menu entries while the itest server syncs, pauses and goes
  offline;
- **Windows:** in the VM, screenshot the notification area (the overflow
  chevron first) and click the icon and menu with `C:\cab\input.ps1`;
- **macOS:** manual.

### P5. Account wizard, Settings and the folder pages (MUST core)

- The wizard (§3.5): server page with TLS fallback and client
  certificate, Login Flow v2, basic auth, sync options, advanced, proxy.
- The re-auth window and the HTTP password prompt.
- The account page (§3.4): the folder list with live progress, inline
  selective sync, the context menu, the big-folder bar, the status label,
  the SSL button, log in/out, remove.
- The folder wizard and the selective sync dialog.
- General, Advanced and Info.
- Connection settings.
- The ignore editors.
- The "Remove all files?" and big-folder prompts.
- The SSL error dialog and proxy authentication.

**Test:**

- `egui_mcp` walk-throughs (as `clap-adn-gui/walk.sh`) against the itest
  server: add an account by Login Flow v2 (the test drives the server-side
  grant with `curl`), choose what to sync, add a second folder, pause,
  resume, force sync, remove;
- after each step, compare `nextcloud.cfg` with the keys and values the
  official client writes for the same actions (a recorded fixture);
- the same walk in the Windows VM.

### P6. Activity, notifications and conflicts

- The OCS notifications and activity jobs in `nc-dav`.
- The Activity page (§3.3) with the status bar, list, actions, dismiss and
  default actions.
- The tray menu's notifications, recent activity and apps.
- Desktop notifications per §5.
- The conflict dialogs, invalid filename and case clash.
- The log browser and debug archive.
- The quota warnings.

**Test:**

- the itest server creates notifications with `occ notification:generate`;
- make conflicts with the existing bench scenarios;
- `egui_mcp` checks the list and dialogs;
- on Linux a test notification daemon on the session bus records `Notify`
  calls; on Windows check the toasts by screenshot.

### P7. Packaging and release

- `-cli` and `-gui` archives per OS.
- Windows signed `.zip`, then an installer that creates the Start-menu
  shortcut with the AppUserModelID.
- macOS signed and notarized `.app` / `.dmg`.
- Linux tarball with `.desktop` and icon (AppImage or Flatpak: decision
  D7).
- README and CHANGELOG.

Release order: as for our other trees, only after the engine milestones
are released.

### LATER (after v1)

User status, unified search, Assistant, file details and sharing, Talk
(chat reply, calls), edit locally (`nc://open`), the updater, the Explorer
navigation pane, libcloudproviders, Windows policies, legacy import, file
manager extensions (socket API), and VFS/E2EE if they are ever in scope.

### 9.1 Linux GUI test bench

Reuse the pattern of `~/.cache/claude-work/clap-adn-gui`:

- a container with Xvfb and `LIBGL_ALWAYS_SOFTWARE=1`, running under the
  host uid, with `--network host`;
- the debug binary built with `--features inspection`, run with
  `EGUI_INSPECTION=1`;
- `egui_mcp` (or the stdio client `guitest/mcp.py`) on 127.0.0.1:5719.

Additions for this project:

- `dbus-daemon --session` inside the container, plus the test SNI watcher
  and notification recorder, so the tray and notifications are asserted
  without a desktop;
- a Secret Service for the keyring (`gnome-keyring-daemon --unlock` with
  an empty password, or a file-backed store in test builds).

### 9.2 Windows VM bench

The disposable `winvm` container (GTX 1080 passthrough, so egui_glow has
real OpenGL), driven through `~/.cache/claude-work/winvm/winssh` (SSH on
127.0.0.1:2222), `winui` (the interactive desktop session) and
`C:\cab\{input,shot}.ps1`.

- **Build:**
  - a quick check with `cargo check --target x86_64-pc-windows-gnu` on the
    host (the cross-platform probe trick);
  - real binaries from the `windows-2025` CI job, or built natively in the
    VM.
- **Server:** the itest server listens on 127.0.0.1:18080 only and must
  stay that way. Reach it from the VM with an SSH remote forward:
  `winssh -R 18080:127.0.0.1:18080 …`, then `http://127.0.0.1:18080`
  inside the VM. Nothing is exposed and the compose file stays untouched.
- **Takeover tests:** install the official v34.0.5 Windows client in the
  VM (it is disposable), set up a classic folder, then
  `ncsync takeover` / GUI takeover and hand-back.
- **Tray:** new icons land in the overflow, so click the chevron first, or
  set "always show" through the `NotifyIconSettings` registry key for the
  test.

### 9.3 Integration server

`tools/itest` (compose project `ncrs-itest`: `nextcloud:34-apache`, redis,
notify_push profile, nginx; 127.0.0.1:18080) stays the single test server.
It is separate from the production Nextcloud and nginx containers on the
same machine, and those must never be used for tests.

GUI tests add:

- a fixture user with an app password;
- `occ notification:generate` for server notifications;
- `files_lock` for lock tests;
- a self-signed TLS variant (an nginx profile with an `rcgen`
  certificate) to drive the SSL error dialog and the "Connect without TLS"
  path.

---

## 10. Decisions

Taken by Sylvain on 2026-11-02.

| # | Question | Decision |
|---|---|---|
| D1 | Product name, binary name, app id | **nc-client**, as the rest of the project already says "nc" for Nextcloud. Downloads `nc-client-cli-<target>` (`ncsync`, `ncsyncd`) and `nc-client-gui-<target>` (`nc-client`). App id `io.github.le_syl21.nc_client` (underscores, so it is also a valid D-Bus name), display name "nc-client", "for Nextcloud" in the descriptions only. |
| D2 | Which configuration file the GUI uses | **The ncsyncd file**: one configuration, the GUI and the daemon are interchangeable on a machine. |
| D3 | When ncsyncd holds the configuration | **Attach, designed in from the start.** The GUI talks to the engine through one interface with two implementations: in-process (it owns the lock) or remote (ncsyncd owns it, the GUI drives it over the control endpoint). The control protocol therefore carries every GUI operation and an event subscription (the `UiEvent` stream) from P0 on, and the GUI never reaches into the engine by another path. |
| D4 | macOS testing | **No Mac will be bought or rented** (revised 2026-11-03). macOS ships as **experimental**: built, signed and unit/integration-tested in CI on GitHub's macOS runners, which is the validation level. Not validated by hand on a real Mac, and said so in the README and docs: the GUI window, the menu-bar tray, the Keychain access prompt and the takeover from the sandboxed official app. No work item depends on a real Mac. |
| D5 | Windows tray right-click | One menu on both clicks. |
| D6 | Translations | Seed from upstream's `client_*.ts` (GPL, attribution) for the strings kept word for word. |
| D7 | Installers | **Binaries first** (archives with the binary, plus the `.desktop` file on Linux); installers later. Without an installer, Windows toasts carry a generic origin until the AppUserModelID is registered by one. |
| D8 | macOS notifications | Deprecated `NSUserNotification` path for v1. |
| D9 | MSRV | Bump to 1.89 for `File::try_lock`. |
| D10 | Sign-out from a GUI attached to ncsyncd | Signing out forgets the stored app password (upstream's invalidateToken), so a headless daemon could only come back through a Login Flow. **When the GUI drives a remote ncsyncd, its "Sign out" action is replaced by "Pause"**; signing out stays available behind an explicit confirmation that says the daemon will need a new login. The in-process GUI keeps upstream's sign-out. |
