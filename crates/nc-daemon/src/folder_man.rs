// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/gui/folderman.{h,cpp}` (nextcloud/desktop v34.0.5),
// with the parts of `src/gui/application.cpp` and `src/gui/accountmanager.cpp`
// that wire the account states to it. Not ported: virtual files, the socket
// API, the macOS file provider, end-to-end encryption, sharing, the
// Windows-only lock watcher and application restart.

//! `FolderMan`: owns the folders and the account states and decides which
//! folder syncs when.
//!
//! * One folder syncs at a time; folders wanting to sync wait in a queue
//!   (`scheduleFolder`), started after a pause based on the duration of the
//!   last sync (`startScheduledSyncSoon`).
//! * Every `remotePollInterval` (30 s by default) the folders whose account
//!   has no push notifications for files check the etag of their remote
//!   root, one request at a time; a changed etag schedules the folder.
//! * Every 5 s folders are scheduled when `forceSyncInterval` (2 h) passed,
//!   or to retry after a failure (10 s, then every 60 s), or when the
//!   engine asked for a delayed follow-up.
//! * A folder failing repeatedly waits 10 s, 30 s or 60 s before it is
//!   queued again.
//! * notify_push `notify_file` schedules all folders of the account,
//!   `notify_file_id` the folders containing one of the file ids.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use nc_dav::Account;
use tokio::sync::mpsc::UnboundedSender;

use crate::account_state::{AccountEvent, AccountSignal, AccountState, EventWrapper, State};
use crate::event::{Event, FolderEvent, FolderId, FolderManEvent, PushEvent, WatcherEvent};
use crate::folder::{Definition, EtagJob, Folder, FolderAction, FolderSettings, WatcherFactory};
use crate::network_limits::NetworkLimits;
use crate::timer::{Timer, single_shot};

const LOG: &str = "nextcloud.gui.folder.manager";

/// The `ConfigFile` values the folder manager reads.
#[derive(Clone, Debug)]
pub struct FolderManSettings {
    /// `remotePollInterval()` (30 s).
    pub remote_poll_interval: Duration,
    /// `forceSyncInterval()` (2 h).
    pub force_sync_interval: Duration,
    /// What every folder reads.
    pub folder: FolderSettings,
}

impl Default for FolderManSettings {
    fn default() -> Self {
        Self {
            remote_poll_interval: Duration::from_secs(30),
            force_sync_interval: Duration::from_secs(2 * 3600),
            folder: FolderSettings::default(),
        }
    }
}

/// Where folder definitions are saved (`Folder::saveToSettings`,
/// `removeFromSettings`).
pub trait FolderSettingsStore {
    /// Saves the definition in the account's `Folders` group (when
    /// `backwards_compatible`) or `Multifolders` group, removing it from
    /// the other groups first.
    fn save_folder(
        &mut self,
        account_id: &str,
        definition: &Definition,
        backwards_compatible: bool,
    );
    /// Removes the folder from all the groups of the account.
    fn remove_folder(&mut self, account_id: &str, alias: &str);
}

/// A store that keeps nothing (tests, `--no-save`).
#[derive(Default)]
pub struct NullSettingsStore;

impl FolderSettingsStore for NullSettingsStore {
    fn save_folder(&mut self, _: &str, _: &Definition, _: bool) {}
    fn remove_folder(&mut self, _: &str, _: &str) {}
}

/// What the folder manager asks its owner (the daemon) to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagerRequest {
    /// The account needs new credentials.
    CredentialsNeeded(String),
}

/// `FolderMan`.
pub struct FolderMan {
    tx: UnboundedSender<Event>,
    settings: FolderManSettings,
    store: Box<dyn FolderSettingsStore>,
    watcher_factory: WatcherFactory,
    /// `AccountManager::accounts()`.
    accounts: BTreeMap<String, AccountState>,
    account_limits: HashMap<String, NetworkLimits>,
    folder_map: BTreeMap<String, Folder>,
    aliases: HashMap<FolderId, String>,
    next_id: FolderId,
    disabled_folders: HashSet<FolderId>,
    current_sync_folder: Option<FolderId>,
    last_sync_folder: Option<FolderId>,
    sync_enabled: bool,
    /// Starts regular etag query jobs
    etag_poll_timer: Timer,
    /// The currently running etag query
    current_etag_job: Option<FolderId>,
    /// Occasionally schedules folders
    time_scheduler: Timer,
    /// Scheduled folders that should be synced as soon as possible
    scheduled_folders: VecDeque<FolderId>,
    /// Picks the next scheduled folder and starts the sync
    start_scheduled_sync_timer: Timer,
    next_sync_should_start_immediately: bool,
    requests: Vec<ManagerRequest>,
    account_wrap: EventWrapper<Event>,
}

impl FolderMan {
    /// `FolderMan()`: starts the etag poll timer and the time scheduler.
    pub fn new(
        tx: &UnboundedSender<Event>,
        settings: FolderManSettings,
        store: Box<dyn FolderSettingsStore>,
        watcher_factory: WatcherFactory,
    ) -> Self {
        let polltime = settings.remote_poll_interval;
        log::info!(target: LOG, "setting remote poll timer interval to {} msec", polltime.as_millis());
        let mut fm = Self {
            tx: tx.clone(),
            settings,
            store,
            watcher_factory,
            accounts: BTreeMap::new(),
            account_limits: HashMap::new(),
            folder_map: BTreeMap::new(),
            aliases: HashMap::new(),
            next_id: 1,
            disabled_folders: HashSet::new(),
            current_sync_folder: None,
            last_sync_folder: None,
            sync_enabled: true,
            etag_poll_timer: Timer::new(polltime, false),
            current_etag_job: None,
            time_scheduler: Timer::new(Duration::from_secs(5), false),
            scheduled_folders: VecDeque::new(),
            start_scheduled_sync_timer: Timer::new(Duration::ZERO, true),
            next_sync_should_start_immediately: false,
            requests: Vec::new(),
            account_wrap: Arc::new(Event::Account),
        };
        fm.etag_poll_timer
            .start(tx, |g| Event::FolderMan(FolderManEvent::EtagPollTimer(g)));
        fm.time_scheduler
            .start(tx, |g| Event::FolderMan(FolderManEvent::TimeScheduler(g)));
        fm
    }

    /// The requests for the owner accumulated since the last call.
    pub fn take_requests(&mut self) -> Vec<ManagerRequest> {
        std::mem::take(&mut self.requests)
    }

    pub fn settings(&self) -> &FolderManSettings {
        &self.settings
    }

    // ---- accounts (AccountManager / Application wiring) ----

    /// `AccountManager::addAccountState` + `Application::slotAccountStateAdded`.
    pub fn add_account(
        &mut self,
        id: &str,
        account: Arc<Account>,
        credentials_ready: bool,
        limits: NetworkLimits,
        push: Option<nc_dav::HttpClientOptions>,
    ) {
        if let Some(options) = push {
            account.enable_push_notifications(options);
            forward_push_events(id, &account, &self.tx);
        }
        let mut state = AccountState::new(
            id,
            account,
            credentials_ready,
            self.settings.remote_poll_interval,
        );
        state.start(&self.tx, self.account_wrap.clone());
        self.accounts.insert(id.to_owned(), state);
        self.account_limits.insert(id.to_owned(), limits);
    }

    /// Adds an account that is connected and never checks its connection
    /// (`FakeAccountState`, for tests).
    pub fn add_fake_connected_account(&mut self, id: &str, account: Arc<Account>) {
        self.accounts
            .insert(id.to_owned(), AccountState::new_fake_connected(id, account));
        self.account_limits
            .insert(id.to_owned(), NetworkLimits::default());
    }

    /// Empties the schedule queue (tests: `_scheduledFolders.clear()`).
    pub fn clear_schedule_queue(&mut self) {
        self.scheduled_folders.clear();
    }

    pub fn account_state(&self, id: &str) -> Option<&AccountState> {
        self.accounts.get(id)
    }

    pub fn account_states(&self) -> impl Iterator<Item = &AccountState> {
        self.accounts.values()
    }

    fn account_connected(&self, id: &str) -> bool {
        self.accounts.get(id).is_some_and(|a| a.is_connected())
    }

    /// New credentials for an account (reloaded from the store).
    pub fn set_account_credentials_ready(&mut self, id: &str, ready: bool) {
        let wrap = self.account_wrap.clone();
        let Some(state) = self.accounts.get_mut(id) else {
            return;
        };
        let signals = state.set_credentials_ready(ready, &self.tx, &wrap);
        self.handle_account_signals(id, signals);
    }

    fn handle_account_event(&mut self, id: &str, event: AccountEvent) {
        let wrap = self.account_wrap.clone();
        let Some(state) = self.accounts.get_mut(id) else {
            return;
        };
        let signals = state.handle_event(event, &self.tx, &wrap);
        self.handle_account_signals(id, signals);
    }

    fn handle_account_signals(&mut self, id: &str, signals: Vec<AccountSignal>) {
        for signal in signals {
            match signal {
                AccountSignal::StateChanged(_) => self.slot_account_state_changed(id),
                // Folder::canSyncChanged → the socket API only.
                AccountSignal::IsConnectedChanged => {}
                AccountSignal::TermsOfServiceChanged(state) => {
                    // Folder: setSyncPaused(state == NeedToSignTermsOfService)
                    let ids: Vec<FolderId> = self
                        .folder_map
                        .values()
                        .filter(|f| f.account_id() == id)
                        .map(|f| f.id())
                        .collect();
                    for fid in ids {
                        self.set_folder_sync_paused(fid, state == State::NeedToSignTermsOfService);
                    }
                }
                AccountSignal::TrySetupPushNotifications => {
                    if let Some(a) = self.accounts.get(id) {
                        a.account().try_setup_push_notifications();
                    }
                }
                AccountSignal::CredentialsNeeded => {
                    log::warn!(target: LOG, "Account {id} needs credentials: run `ncsync account login` (or provide the systemd credential) and reload the daemon");
                    self.requests
                        .push(ManagerRequest::CredentialsNeeded(id.to_owned()));
                }
            }
        }
    }

    /// `slotAccountStateChanged()`: schedules folders of newly connected
    /// accounts, terminates and de-schedules folders of disconnected
    /// accounts.
    fn slot_account_state_changed(&mut self, account_id: &str) {
        let Some(state) = self.accounts.get(account_id) else {
            return;
        };
        let account_name = state.account().dav_display_name();
        if state.is_connected() {
            log::info!(target: LOG, "Account {account_name} connected, scheduling its folders");
            let ids: Vec<FolderId> = self
                .folder_map
                .values()
                .filter(|f| f.account_id() == account_id && f.can_sync(true))
                .map(|f| f.id())
                .collect();
            for id in ids {
                self.schedule_folder(id);
            }
        } else {
            log::info!(target: LOG, "Account {account_name} disconnected or paused, terminating or descheduling sync folders");
            for f in self.folder_map.values_mut() {
                if f.is_sync_running() && f.account_id() == account_id {
                    f.slot_terminate_sync();
                }
            }
            let aliases = &self.aliases;
            let map = &self.folder_map;
            self.scheduled_folders.retain(|id| {
                aliases
                    .get(id)
                    .and_then(|a| map.get(a))
                    .is_some_and(|f| f.account_id() != account_id)
            });
        }
    }

    // ---- folders ----

    /// `map()`.
    pub fn map(&self) -> &BTreeMap<String, Folder> {
        &self.folder_map
    }

    /// `folder(alias)`.
    pub fn folder(&self, alias: &str) -> Option<&Folder> {
        self.folder_map.get(alias)
    }

    pub fn folder_mut(&mut self, alias: &str) -> Option<&mut Folder> {
        self.folder_map.get_mut(alias)
    }

    fn folder_by_id(&self, id: FolderId) -> Option<&Folder> {
        self.aliases.get(&id).and_then(|a| self.folder_map.get(a))
    }

    fn folder_by_id_mut(&mut self, id: FolderId) -> Option<&mut Folder> {
        let alias = self.aliases.get(&id)?;
        self.folder_map.get_mut(alias)
    }

    /// `scheduleQueue()`: the aliases of the queued folders.
    pub fn schedule_queue(&self) -> Vec<String> {
        self.scheduled_folders
            .iter()
            .filter_map(|id| self.aliases.get(id).cloned())
            .collect()
    }

    /// `currentSyncFolder()`.
    pub fn current_sync_folder(&self) -> Option<&str> {
        self.current_sync_folder
            .and_then(|id| self.aliases.get(&id))
            .map(String::as_str)
    }

    /// `addFolderInternal` (the folder of a loaded definition): registers
    /// the folder, its watcher, and schedules it (`setupFoldersHelper`).
    /// Returns the folder's alias (made unique like upstream).
    pub fn add_folder_from_settings(
        &mut self,
        account_id: &str,
        definition: Definition,
        backwards_compatible: bool,
    ) -> Option<String> {
        let alias = self.add_folder_internal(account_id, definition)?;
        let id = self.folder_map[&alias].id();
        if backwards_compatible && let Some(f) = self.folder_map.get_mut(&alias) {
            // Migration: Mark folders that shall be saved in a backwards-compatible way
            f.set_save_backwards_compatible(true);
        }
        self.schedule_folder(id);
        Some(alias)
    }

    /// `addFolder(accountState, folderDefinition)`: a new folder, saved in
    /// the settings.
    pub fn add_folder(&mut self, account_id: &str, mut definition: Definition) -> Option<String> {
        // Choose a db filename
        let account = self.accounts.get(account_id)?.account().clone();
        definition.journal_path = nc_journal::journal::make_db_name(
            std::path::Path::new(&definition.local_path),
            &account.url().to_credential_free_string(),
            &definition.target_path,
            &account.credentials().user,
        );
        if !ensure_journal_gone(&definition.absolute_journal_path()) {
            return None;
        }
        let clean = nc_sync::touched_files::clean_path(&definition.local_path);
        let alias = self.add_folder_internal(account_id, definition)?;
        // Migration: The first account that's configured for a local folder shall
        // be saved in a backwards-compatible way.
        let one_account_only = !self
            .folder_map
            .values()
            .any(|other| other.alias() != alias && other.clean_path() == clean);
        if let Some(f) = self.folder_map.get_mut(&alias) {
            f.set_save_backwards_compatible(one_account_only);
        }
        self.save_folder_to_settings(&alias);
        Some(alias)
    }

    fn add_folder_internal(
        &mut self,
        account_id: &str,
        mut definition: Definition,
    ) -> Option<String> {
        let account = self.accounts.get(account_id)?.account().clone();
        let base = definition.alias.clone();
        let mut count = 0i64;
        while definition.alias.is_empty() || self.folder_map.contains_key(&definition.alias) {
            // There is already a folder configured with this name and folder names need to be unique
            count += 1;
            definition.alias = (nc_sync::utility::qstring_to_int(&base) + count).to_string();
        }
        let id = self.next_id;
        self.next_id += 1;
        let limits = self
            .account_limits
            .get(account_id)
            .copied()
            .unwrap_or_default();
        let mut folder = Folder::new(
            id,
            definition,
            account_id,
            account,
            self.settings.folder.clone(),
            limits,
            &self.tx,
        );
        let alias = folder.alias().to_owned();
        log::info!(target: LOG, "Adding folder to Folder Map  {alias}");
        if folder.sync_paused() {
            self.disabled_folders.insert(id);
        }
        folder.register_folder_watcher(&self.watcher_factory);
        self.aliases.insert(id, alias.clone());
        self.folder_map.insert(alias.clone(), folder);
        Some(alias)
    }

    /// `Folder::saveToSettings()`.
    fn save_folder_to_settings(&mut self, alias: &str) {
        let Some(f) = self.folder_map.get(alias) else {
            return;
        };
        // True if the folder path appears in only one account
        let clean = f.clean_path();
        let one_account_only = !self
            .folder_map
            .values()
            .any(|other| other.alias() != alias && other.clean_path() == clean);
        let backwards = f.save_backwards_compatible() || one_account_only;
        let (account_id, definition) = (f.account_id().to_owned(), f.definition().clone());
        self.store.remove_folder(&account_id, alias);
        self.store.save_folder(&account_id, &definition, backwards);
        log::info!(target: LOG, "Saved folder {alias} to settings");
    }

    /// `removeFolder(folder)`.
    pub fn remove_folder(&mut self, alias: &str) {
        let Some(mut f) = self.folder_map.remove(alias) else {
            log::error!(target: LOG, "Can not remove null folder");
            return;
        };
        log::info!(target: LOG, "Removing  {alias}");
        let id = f.id();
        let currently_running = f.is_sync_running();
        if currently_running {
            // abort the sync now
            f.slot_terminate_sync();
        }
        self.scheduled_folders.retain(|x| *x != id);
        let _ = f.set_sync_paused(true);
        f.wipe_for_removal();
        // remove the folder configuration
        self.store.remove_folder(f.account_id(), alias);
        self.disabled_folders.remove(&id);
        self.aliases.remove(&id);
        if self.current_etag_job == Some(id) {
            self.current_etag_job = None;
        }
        // A running folder finishes in the background; its EngineFinished
        // then schedules the next folder (slotFolderSyncFinished).
    }

    /// `unloadAndDeleteAllFolders()`.
    pub fn unload_and_delete_all_folders(&mut self) -> usize {
        let cnt = self.folder_map.len();
        for f in self.folder_map.values_mut() {
            if f.is_sync_running() {
                f.slot_terminate_sync();
            }
        }
        self.folder_map.clear();
        self.aliases.clear();
        self.disabled_folders.clear();
        self.last_sync_folder = None;
        self.current_sync_folder = None;
        self.scheduled_folders.clear();
        cnt
    }

    /// `folderForPath(path)`.
    pub fn folder_for_path(&self, path: &str) -> Option<&Folder> {
        let absolute_path = format!("{}/", nc_sync::touched_files::clean_path(path));
        self.folder_map.values().find(|f| {
            let folder_path = format!("{}/", f.clean_path());
            absolute_path.starts_with(&folder_path)
        })
    }

    // ---- scheduling ----

    /// `scheduleAllFolders()`.
    pub fn schedule_all_folders(&mut self) {
        let ids: Vec<FolderId> = self
            .folder_map
            .values()
            .filter(|f| f.can_sync(self.account_connected(f.account_id())))
            .map(|f| f.id())
            .collect();
        for id in ids {
            self.schedule_folder(id);
        }
    }

    /// `forceSyncForFolder(folder)`.
    pub fn force_sync_for_folder(&mut self, alias: &str) {
        // Terminate and reschedule any running sync
        let running: Vec<FolderId> = self
            .folder_map
            .values()
            .filter(|f| f.is_sync_running())
            .map(|f| f.id())
            .collect();
        for id in running {
            if let Some(f) = self.folder_by_id_mut(id) {
                f.slot_terminate_sync();
            }
            self.schedule_folder(id);
        }
        let Some(f) = self.folder_map.get(alias) else {
            return;
        };
        let id = f.id();
        f.slot_wipe_error_blacklist(); // issue #6757
        self.set_folder_sync_paused(id, false);
        // Insert the selected folder at the front of the queue
        self.schedule_folder_next(id);
    }

    /// `Folder::setSyncPaused(paused)` with its signals.
    pub fn set_folder_sync_paused(&mut self, id: FolderId, paused: bool) {
        let Some(f) = self.folder_by_id_mut(id) else {
            return;
        };
        let actions = f.set_sync_paused(paused);
        self.run_actions(id, actions);
    }

    /// `scheduleFolder(folder)`: if a folder wants to be synced, it calls
    /// this slot and is added to the queue. The slot to actually start a
    /// sync is called afterwards.
    pub fn schedule_folder(&mut self, id: FolderId) {
        let Some(f) = self.folder_by_id(id) else {
            log::error!(target: LOG, "slotScheduleSync called with null folder");
            return;
        };
        let alias = f.alias().to_owned();
        log::info!(target: LOG, "Schedule folder  {alias}  to sync!");
        let failing = f.consecutive_failing_syncs();
        let sync_again_delay = if failing > 2 && failing <= 4 {
            Duration::from_secs(10)
        } else if failing > 4 && failing <= 6 {
            Duration::from_secs(30)
        } else if failing > 6 {
            Duration::from_secs(60)
        } else {
            Duration::ZERO
        };
        if !self.scheduled_folders.contains(&id) {
            if !f.can_sync(self.account_connected(f.account_id())) {
                log::info!(target: LOG, "Folder is not ready to sync, not scheduled!");
                return;
            }
            if sync_again_delay.is_zero() {
                self.enqueue(id);
            } else {
                log::warn!(target: LOG, "going to delay the next sync run due to too many synchronization errors {}s", sync_again_delay.as_secs());
                single_shot(
                    sync_again_delay,
                    &self.tx,
                    Event::FolderMan(FolderManEvent::DelayedEnqueue(id)),
                );
            }
        } else {
            log::info!(target: LOG, "Sync for folder  {alias}  already scheduled, do not enqueue!");
            if sync_again_delay.is_zero() {
                self.start_scheduled_sync_soon();
            } else {
                log::warn!(target: LOG, "going to delay the next sync run due to too many synchronization errors {}s", sync_again_delay.as_secs());
                single_shot(
                    sync_again_delay,
                    &self.tx,
                    Event::FolderMan(FolderManEvent::DelayedStartSoon),
                );
            }
        }
    }

    fn enqueue(&mut self, id: FolderId) {
        let Some(f) = self.folder_by_id_mut(id) else {
            return;
        };
        f.prepare_to_sync();
        self.scheduled_folders.push_back(id);
        self.start_scheduled_sync_soon();
    }

    /// `scheduleFolderForImmediateSync(folder)`.
    pub fn schedule_folder_for_immediate_sync(&mut self, id: FolderId) {
        self.next_sync_should_start_immediately = true;
        self.schedule_folder(id);
    }

    /// `scheduleFolderNext(folder)`: puts a folder in the very front of the
    /// queue.
    pub fn schedule_folder_next(&mut self, id: FolderId) {
        let connected = self
            .folder_by_id(id)
            .is_some_and(|f| self.account_connected(f.account_id()));
        let Some(f) = self.folder_by_id_mut(id) else {
            return;
        };
        log::info!(target: LOG, "Schedule folder  {}  to sync! Front-of-queue.", f.alias());
        if !f.can_sync(connected) {
            log::info!(target: LOG, "Folder is not ready to sync, not scheduled!");
            return;
        }
        f.prepare_to_sync();
        self.scheduled_folders.retain(|x| *x != id);
        self.scheduled_folders.push_front(id);
        self.start_scheduled_sync_soon();
    }

    /// `slotFolderSyncPaused(folder, paused)`.
    fn slot_folder_sync_paused(&mut self, id: FolderId, paused: bool) {
        if !paused {
            self.disabled_folders.remove(&id);
            self.schedule_folder(id);
        } else {
            self.disabled_folders.insert(id);
        }
    }

    /// `setSyncEnabled(enabled)`: only enable or disable foldermans will
    /// schedule and do syncs. This is not the same as Pause and Resume of
    /// folders.
    pub fn set_sync_enabled(&mut self, enabled: bool) {
        if !self.sync_enabled && enabled && !self.scheduled_folders.is_empty() {
            // We have things in our queue that were waiting for the connection to come back on.
            self.start_scheduled_sync_soon();
        }
        self.sync_enabled = enabled;
    }

    /// `isAnySyncRunning()`.
    pub fn is_any_sync_running(&self) -> bool {
        self.current_sync_folder.is_some() || self.folder_map.values().any(|f| f.is_sync_running())
    }

    /// `startScheduledSyncSoon()`: will start a sync after a bit of delay.
    fn start_scheduled_sync_soon(&mut self) {
        if self.start_scheduled_sync_timer.is_active() {
            return;
        }
        if self.scheduled_folders.is_empty() {
            return;
        }
        if self.is_any_sync_running() {
            return;
        }
        let mut ms_delay: i64 = 100; // 100ms minimum delay
        let mut ms_since_last_sync: i64 = 0;
        // Require a pause based on the duration of the last sync run.
        if let Some(last_folder) = self.last_sync_folder.and_then(|id| self.folder_by_id(id)) {
            ms_since_last_sync = last_folder.msec_since_last_sync().as_millis() as i64;
            //  1s   -> 1.5s pause
            // 10s   -> 5s pause
            //  1min -> 12s pause
            //  1h   -> 90s pause
            let pause = ((last_folder.msec_last_sync_duration().as_millis() as f64).sqrt() / 20.0
                * 1000.0) as i64;
            ms_delay = ms_delay.max(pause);
        }
        // Delays beyond one minute seem too big, particularly since there
        // could be things later in the queue that shouldn't be punished by a
        // long delay!
        ms_delay = ms_delay.min(60 * 1000);
        // Time since the last sync run counts against the delay
        ms_delay = 1.max(ms_delay - ms_since_last_sync);
        if self.next_sync_should_start_immediately {
            self.next_sync_should_start_immediately = false;
            log::info!(target: LOG, "Next sync is marked to start immediately, so setting the delay to '0'");
            ms_delay = 0;
        }
        log::info!(target: LOG, "Starting the next scheduled sync in {} seconds", ms_delay / 1000);
        self.start_scheduled_sync_timer.start_with(
            Duration::from_millis(ms_delay as u64),
            &self.tx,
            |g| Event::FolderMan(FolderManEvent::StartScheduledSyncTimer(g)),
        );
    }

    /// `slotStartScheduledFolderSync()`.
    fn slot_start_scheduled_folder_sync(&mut self) {
        if self.is_any_sync_running() {
            for f in self.folder_map.values() {
                if f.is_sync_running() {
                    log::info!(target: LOG, "Currently folder  {}  is running, wait for finish!", f.remote_url());
                }
            }
            return;
        }
        if !self.sync_enabled {
            log::info!(target: LOG, "FolderMan: Syncing is disabled, no scheduling.");
            return;
        }
        log::debug!(target: LOG, "folderQueue size:  {}", self.scheduled_folders.len());
        // Find the first folder in the queue that can be synced.
        let mut folder = None;
        while let Some(g) = self.scheduled_folders.pop_front() {
            if let Some(f) = self.folder_by_id(g)
                && f.can_sync(self.account_connected(f.account_id()))
            {
                folder = Some(g);
                break;
            }
        }
        // Start syncing this folder!
        if let Some(id) = folder {
            let factory = self.watcher_factory.clone();
            let Some(f) = self.folder_by_id_mut(id) else {
                return;
            };
            // Safe to call several times, and necessary to try again if
            // the folder path didn't exist previously.
            f.register_folder_watcher(&factory);
            let actions = f.start_sync();
            self.current_sync_folder = Some(id);
            self.run_actions(id, actions);
        }
    }

    /// `pushNotificationsFilesReady(account)`.
    fn push_notifications_files_ready(&self, account_id: &str) -> bool {
        self.accounts.get(account_id).is_some_and(|a| {
            let account = a.account();
            account.capabilities().push_notifications_files_available()
                && account.push_notifications().is_some_and(|p| p.is_ready())
        })
    }

    /// `slotEtagPollTimerTimeout()`.
    fn slot_etag_poll_timer_timeout(&mut self) {
        log::info!(target: LOG, "Etag poll timer timeout");
        log::info!(target: LOG, "Folders to sync: {}", self.folder_map.len());
        // Some folders need not to be checked because they use the push notifications
        let folders_to_run: Vec<FolderId> = self
            .folder_map
            .values()
            .filter(|f| !self.push_notifications_files_ready(f.account_id()))
            .map(|f| f.id())
            .collect();
        log::info!(target: LOG, "Number of folders that don't use push notifications: {}", folders_to_run.len());
        for id in folders_to_run {
            self.run_etag_job_if_possible(id);
        }
    }

    /// `runEtagJobIfPossible(folder)`.
    fn run_etag_job_if_possible(&mut self, id: FolderId) {
        let polltime = self.settings.remote_poll_interval;
        let Some(f) = self.folder_by_id(id) else {
            return;
        };
        log::info!(target: LOG, "Run etag job on folder {}", f.alias());
        if f.is_sync_running() {
            log::info!(target: LOG, "Can not run etag job: Sync is running");
            return;
        }
        if self.scheduled_folders.contains(&id) {
            log::info!(target: LOG, "Can not run etag job: Folder is already scheduled");
            return;
        }
        if self.disabled_folders.contains(&id) {
            log::info!(target: LOG, "Can not run etag job: Folder is disabled");
            return;
        }
        if f.etag_job() != EtagJob::None
            || f.is_busy()
            || !f.can_sync(self.account_connected(f.account_id()))
        {
            log::info!(target: LOG, "Can not run etag job: Folder is busy");
            return;
        }
        // When not using push notifications, make sure polltime is reached
        if !self.push_notifications_files_ready(f.account_id())
            && f.msec_since_last_sync() < polltime
        {
            log::info!(target: LOG, "Can not run etag job: Polltime not reached");
            return;
        }
        // QMetaObject::invokeMethod(folder, "slotRunEtagJob", Qt::QueuedConnection)
        let _ = self.tx.send(Event::Folder(id, FolderEvent::RunEtagJob));
    }

    /// `slotRunOneEtagJob()`.
    fn slot_run_one_etag_job(&mut self) {
        if self.current_etag_job.is_some() {
            return;
        }
        // Caveat: always grabs the first folder with a job, but we think this
        // is Ok for now and avoids us having a separate queue.
        let next = self
            .folder_map
            .values()
            .find(|f| f.etag_job() == EtagJob::Queued)
            .map(|f| f.id());
        if let Some(id) = next {
            self.current_etag_job = Some(id);
            if let Some(f) = self.folder_by_id_mut(id) {
                log::debug!(target: LOG, "Scheduling {} to check remote ETag", f.remote_url());
                f.start_etag_job(); // on destroy/end it will continue the queue via slotEtagJobDestroyed
            }
        }
    }

    /// `slotScheduleFolderByTime()`: schedules folders whose time to sync
    /// has come, either because a long time has passed since the last sync
    /// or because of previous failures.
    fn slot_schedule_folder_by_time(&mut self) {
        let mut to_schedule = Vec::new();
        for f in self.folder_map.values() {
            // Never schedule if syncing is disabled or when we're currently
            // querying the server for etags
            if !f.can_sync(self.account_connected(f.account_id())) || f.etag_job() != EtagJob::None
            {
                continue;
            }
            let msecs_since_sync = f.msec_since_last_sync();
            // Possibly it's just time for a new sync run
            let force_sync_interval_expired = msecs_since_sync > self.settings.force_sync_interval;
            if force_sync_interval_expired {
                log::info!(target: LOG, "Scheduling folder {} because it has been {}ms  since the last sync", f.alias(), msecs_since_sync.as_millis());
                to_schedule.push(f.id());
                continue;
            }
            // Retry a couple of times after failure; or regularly if requested
            let failing = f.consecutive_failing_syncs();
            let sync_again = (failing > 0 && failing < 3)
                || f.another_sync_needed() == nc_sync::AnotherSyncNeeded::DelayedFollowUp;
            let sync_again_delay = if failing > 1 {
                Duration::from_secs(60) // 60s for each further attempt
            } else {
                Duration::from_secs(10) // 10s for the first retry-after-fail
            };
            if sync_again && msecs_since_sync > sync_again_delay {
                log::info!(target: LOG, "Scheduling folder {} , the last {failing} syncs failed , anotherSyncNeeded {:?} , last status: {} , time since last sync: {}", f.alias(), f.another_sync_needed(), f.sync_result().status_string(), msecs_since_sync.as_millis());
                to_schedule.push(f.id());
                continue;
            }
            // Do we want to retry failing syncs or another-sync-needed runs more often?
        }
        for id in to_schedule {
            self.schedule_folder(id);
        }
    }

    /// `slotFolderSyncFinished(result)`: a folder indicates that its syncing
    /// is finished. Start the next sync after the system had some
    /// milliseconds to breath. This delay is particularly useful to avoid
    /// late file change notifications (that we caused ourselves by syncing)
    /// from triggering another spurious sync.
    fn slot_folder_sync_finished(&mut self, id: FolderId) {
        if let Some(f) = self.folder_by_id(id) {
            log::info!(target: LOG, "<========== Sync finished for folder [{}] of account [{}] with remote [{}]", f.path(), f.account().dav_display_name(), f.remote_url());
        }
        if Some(id) == self.current_sync_folder {
            self.last_sync_folder = self.current_sync_folder.take();
        }
        if !self.is_any_sync_running() {
            self.start_scheduled_sync_soon();
        }
    }

    /// `slotProcessFilesPushNotification(account)`.
    pub fn slot_process_files_push_notification(&mut self, account_id: &str) {
        log::debug!(target: LOG, "received notify_file push notification account={account_id}");
        let ids: Vec<FolderId> = self
            .folder_map
            .values()
            // Just run on the folders that belong to this account
            .filter(|f| f.account_id() == account_id)
            .map(|f| f.id())
            .collect();
        for id in ids {
            if let Some(f) = self.folder_by_id(id) {
                log::info!(target: LOG, "scheduling sync folder={} account={account_id} reason=notify_file", f.alias());
            }
            self.schedule_folder(id);
        }
    }

    /// `slotProcessFileIdsPushNotification(account, fileIds)`.
    pub fn slot_process_file_ids_push_notification(&mut self, account_id: &str, file_ids: &[i64]) {
        log::debug!(target: LOG, "received notify_file_id push notification account={account_id} fileIds={file_ids:?}");
        let mut ids = Vec::new();
        for f in self.folder_map.values() {
            // Just run on the folders that belong to this account
            if f.account_id() != account_id {
                continue;
            }
            if !f.has_file_ids(file_ids) {
                log::debug!(target: LOG, "no matching file ids, ignoring folder={} account={account_id}", f.alias());
                continue;
            }
            log::info!(target: LOG, "scheduling sync folder={} account={account_id} reason=notify_file_id", f.alias());
            ids.push(f.id());
        }
        for id in ids {
            self.schedule_folder(id);
        }
    }

    /// Account-level push events (`AccountState::slotPushNotificationsReady`,
    /// `FolderMan::slotConnectToPushNotifications` and the files signals).
    fn handle_push_event(&mut self, account_id: &str, event: PushEvent) {
        match event {
            PushEvent::Ready => {
                log::info!(target: LOG, "Push notifications ready");
                let wrap = self.account_wrap.clone();
                if let Some(state) = self.accounts.get_mut(account_id) {
                    let signals = state.slot_push_notifications_ready(&self.tx, &wrap);
                    self.handle_account_signals(account_id, signals);
                }
            }
            PushEvent::Disabled => {
                if let Some(state) = self.accounts.get_mut(account_id) {
                    state.set_push_notifications_ready(false);
                }
            }
            // slotConnectToPushNotifications connects these only while
            // pushNotificationsFilesReady(account).
            PushEvent::FilesChanged if self.push_notifications_files_ready(account_id) => {
                self.slot_process_files_push_notification(account_id)
            }
            PushEvent::FileIdsChanged(ids) if self.push_notifications_files_ready(account_id) => {
                self.slot_process_file_ids_push_notification(account_id, &ids)
            }
            PushEvent::FilesChanged | PushEvent::FileIdsChanged(_) => {}
        }
    }

    /// `setDirtyNetworkLimits(account)`: new bandwidth limits for an
    /// account (applied by the folders at their next sync).
    pub fn set_network_limits(&mut self, account_id: &str, limits: NetworkLimits) {
        self.account_limits.insert(account_id.to_owned(), limits);
        for f in self.folder_map.values_mut() {
            if f.account_id() == account_id {
                f.set_network_limits(limits);
            }
        }
    }

    /// The `FolderAction`s of a folder method: the signal connections.
    fn run_actions(&mut self, id: FolderId, actions: Vec<FolderAction>) {
        for action in actions {
            match action {
                FolderAction::Schedule => self.schedule_folder(id),
                FolderAction::ScheduleImmediate => self.schedule_folder_for_immediate_sync(id),
                FolderAction::SetSyncEnabled(enabled) => self.set_sync_enabled(enabled),
                FolderAction::EtagJobCreated => {
                    // slotScheduleETagJob: QMetaObject::invokeMethod(this, "slotRunOneEtagJob", Qt::QueuedConnection)
                    let _ = self
                        .tx
                        .send(Event::FolderMan(FolderManEvent::RunOneEtagJob));
                }
                FolderAction::TagLastSuccessfulEtagRequest(t) => {
                    let account_id = self.folder_by_id(id).map(|f| f.account_id().to_owned());
                    if let Some(a) = account_id.and_then(|a| self.accounts.get_mut(&a)) {
                        a.tag_last_successful_etag_request(t);
                    }
                }
                FolderAction::SyncStarted => {
                    // slotFolderSyncStarted
                    if let Some(f) = self.folder_by_id(id) {
                        log::info!(target: LOG, ">========== Sync started for folder [{}] of account [{}] with remote [{}]", f.path(), f.account().dav_display_name(), f.remote_url());
                    }
                }
                FolderAction::SyncFinished => self.slot_folder_sync_finished(id),
                FolderAction::SyncStateChange | FolderAction::CanSyncChanged => {}
                FolderAction::SyncPausedChanged(paused) => self.slot_folder_sync_paused(id, paused),
                FolderAction::SaveToSettings => {
                    if let Some(alias) = self.aliases.get(&id).cloned() {
                        self.save_folder_to_settings(&alias);
                    }
                }
            }
        }
    }

    /// Handles one event of the queue.
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Account(id, e) => self.handle_account_event(&id, e),
            Event::FolderMan(e) => self.handle_folder_man_event(e),
            Event::Folder(id, e) => self.handle_folder_event(id, e),
            Event::Watcher(id, e) => self.handle_watcher_event(id, e),
            Event::Push(account_id, e) => self.handle_push_event(&account_id, e),
            Event::Control(..)
            | Event::Shutdown
            | Event::WatchdogTick
            | Event::ReloadCredentials => {}
        }
    }

    fn handle_folder_man_event(&mut self, event: FolderManEvent) {
        match event {
            FolderManEvent::EtagPollTimer(g) => {
                if self.etag_poll_timer.fired(g) {
                    self.slot_etag_poll_timer_timeout();
                }
            }
            FolderManEvent::TimeScheduler(g) => {
                if self.time_scheduler.fired(g) {
                    self.slot_schedule_folder_by_time();
                }
            }
            FolderManEvent::StartScheduledSyncTimer(g) => {
                if self.start_scheduled_sync_timer.fired(g) {
                    self.slot_start_scheduled_folder_sync();
                }
            }
            FolderManEvent::RunOneEtagJob => self.slot_run_one_etag_job(),
            FolderManEvent::DelayedEnqueue(id) => self.enqueue(id),
            FolderManEvent::DelayedStartSoon => self.start_scheduled_sync_soon(),
        }
    }

    fn handle_watcher_event(&mut self, id: FolderId, event: WatcherEvent) {
        let Some(f) = self.folder_by_id_mut(id) else {
            return;
        };
        let actions = f.handle_watcher_event(event);
        self.run_actions(id, actions);
    }

    /// A folder's own `startSync()` call (the engine's scheduled sync run
    /// timers, the lock file requests): it bypasses the queue upstream; here
    /// the folder is queued when another folder is syncing, since only one
    /// sync runs at a time.
    fn start_folder_sync_directly(&mut self, id: FolderId, reason: &str) {
        if self.is_any_sync_running() {
            self.schedule_folder(id);
            return;
        }
        let factory = self.watcher_factory.clone();
        let actions = match self.folder_by_id_mut(id) {
            Some(f) => {
                log::info!(target: LOG, "Rescanning {} {reason}", f.alias());
                f.register_folder_watcher(&factory);
                f.start_sync()
            }
            None => return,
        };
        self.current_sync_folder = Some(id);
        self.run_actions(id, actions);
    }

    fn handle_folder_event(&mut self, id: FolderId, event: FolderEvent) {
        if self.folder_by_id(id).is_none() {
            // A removed folder: its engine comes back once, and the next
            // folder can start (removeFolder connects syncFinished).
            if let FolderEvent::EngineFinished(..) = event
                && self.current_sync_folder == Some(id)
            {
                self.current_sync_folder = None;
                self.start_scheduled_sync_soon();
            }
            return;
        }
        let connected = self
            .folder_by_id(id)
            .is_some_and(|f| self.account_connected(f.account_id()));
        let event = match event {
            FolderEvent::EtagJobFinished(result, time) => {
                let actions = match self.folder_by_id_mut(id) {
                    Some(f) => f.etag_job_finished(result, time),
                    None => Vec::new(),
                };
                // slotEtagJobDestroyed
                if self.current_etag_job == Some(id) {
                    self.current_etag_job = None;
                }
                let _ = self
                    .tx
                    .send(Event::FolderMan(FolderManEvent::RunOneEtagJob));
                self.run_actions(id, actions);
                return;
            }
            FolderEvent::ScheduledSyncTimer(g) => {
                let fire = self
                    .folder_by_id_mut(id)
                    .is_some_and(|f| f.scheduled_sync_timer_fired(g));
                if !fire {
                    return;
                }
                self.start_folder_sync_directly(id, "for files whose lock expired");
                return;
            }
            FolderEvent::LockFileStateFinished(finished) => {
                let start_sync = self
                    .folder_by_id_mut(id)
                    .is_some_and(|f| f.lock_file_state_finished(&finished));
                if start_sync {
                    self.start_folder_sync_directly(id, "after a file lock change");
                }
                return;
            }
            FolderEvent::LockFileDetected(lock_file) => {
                if let Some(f) = self.folder_by_id_mut(id) {
                    f.slot_lock_file_detected(&lock_file);
                }
                return;
            }
            e => e,
        };
        let Some(f) = self.folder_by_id_mut(id) else {
            return;
        };
        let actions = match event {
            FolderEvent::EtagJobFinished(..)
            | FolderEvent::ScheduledSyncTimer(_)
            | FolderEvent::LockFileStateFinished(_)
            | FolderEvent::LockFileDetected(_) => Vec::new(),
            FolderEvent::ScheduleSelfTimer(g) => {
                if f.schedule_self_timer_fired(g) {
                    vec![FolderAction::Schedule]
                } else {
                    Vec::new()
                }
            }
            FolderEvent::EmitFinishedDelayed => f.slot_emit_finished_delayed(connected),
            FolderEvent::RunEtagJob => f.slot_run_etag_job(connected),
            FolderEvent::EngineStarted => f.slot_sync_started(),
            FolderEvent::ItemCompleted(item, _) => {
                f.slot_item_completed(&item);
                Vec::new()
            }
            FolderEvent::TransmissionProgress(pi) => {
                f.slot_transmission_progress(*pi);
                Vec::new()
            }
            FolderEvent::SyncError(msg, _) => {
                f.slot_sync_error(&msg);
                Vec::new()
            }
            FolderEvent::RootEtag(etag, time) => f.etag_retrieved_from_sync_engine(&etag, time),
            FolderEvent::RootFileId(file_id) => {
                f.root_file_id_received_from_sync_engine(file_id);
                Vec::new()
            }
            FolderEvent::NewBigFolder(path, external) => {
                f.new_big_folder_message(&path, external);
                Vec::new()
            }
            FolderEvent::ExistingFolderNowBig(path) => {
                f.slot_existing_folder_now_big(&path);
                Vec::new()
            }
            FolderEvent::AllFilesDeletedRestore => f.all_files_deleted_restore(),
            FolderEvent::EngineFinished(engine, success) => f.engine_finished(*engine, success),
        };
        self.run_actions(id, actions);
    }

    /// Stops everything (`Application::slotCleanup`): aborts running syncs.
    pub fn shutdown(&mut self) {
        self.etag_poll_timer.stop();
        self.time_scheduler.stop();
        self.start_scheduled_sync_timer.stop();
        self.sync_enabled = false;
        for f in self.folder_map.values_mut() {
            f.slot_terminate_sync();
        }
    }

    /// Whether a folder's sync is still running (for a clean shutdown).
    pub fn any_engine_away(&self) -> bool {
        self.folder_map.values().any(|f| f.is_sync_running())
    }
}

/// A watcher factory creating no watcher (tests, `--no-watch`): local
/// changes are then only found by full local discoveries.
pub fn no_watcher() -> WatcherFactory {
    Rc::new(|_, _, _, _| None)
}

/// `FolderMan::slotSetupPushNotifications`: listens to the account's push
/// notification signals and posts them to the queue.
fn forward_push_events(id: &str, account: &Arc<Account>, tx: &UnboundedSender<Event>) {
    use nc_dav::account::AccountPushEvent;
    use nc_dav::push_notifications::PushNotificationsEvent;
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = account.subscribe_push_notifications_events();
    let tx = tx.clone();
    let id = id.to_owned();
    tokio::task::spawn_local(async move {
        loop {
            let event = match rx.recv().await {
                Ok(AccountPushEvent::PushNotificationsReady { .. }) => PushEvent::Ready,
                Ok(AccountPushEvent::PushNotificationsDisabled { .. }) => PushEvent::Disabled,
                Ok(AccountPushEvent::PushNotification(PushNotificationsEvent::FilesChanged {
                    ..
                })) => PushEvent::FilesChanged,
                Ok(AccountPushEvent::PushNotification(
                    PushNotificationsEvent::FileIdsChanged { file_ids, .. },
                )) => PushEvent::FileIdsChanged(file_ids),
                Ok(_) => continue,
                // Missed notifications: sync everything of the account.
                Err(RecvError::Lagged(_)) => PushEvent::FilesChanged,
                Err(RecvError::Closed) => return,
            };
            if tx.send(Event::Push(id.clone(), event)).is_err() {
                return;
            }
        }
    });
}

/// `ensureJournalGone(journalDbFile)`: removes the old journal file (upstream
/// asks to retry with a dialog when it cannot).
pub fn ensure_journal_gone(journal_db_file: &str) -> bool {
    let path = std::path::Path::new(journal_db_file);
    if path.exists()
        && let Err(e) = std::fs::remove_file(path)
    {
        log::warn!(target: LOG, "Could not remove old db file at {journal_db_file}: {e}");
        return false;
    }
    true
}
