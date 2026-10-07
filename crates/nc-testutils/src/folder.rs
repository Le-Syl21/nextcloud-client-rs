/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-FileCopyrightText: 2020 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! `FakeFolder`: a local temporary directory, a [`FakeServer`] seeded from
//! the same template tree, and a sync engine between them.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, MutexGuard};

use nc_dav::{Account, Capabilities, Request, ServerUrl};
use nc_journal::journal::SyncJournalDb;
use nc_sync::{ErrorCategory, SyncEngine, SyncFileItem, SyncFileItemPtr, SyncOptions};

use crate::disk::{DiskFileModifier, from_disk, to_disk};
use crate::file_info::FileInfo;
use crate::path::PathComponents;
use crate::server::{FakeReply, FakeServer, ServerState};

/// `FakeFolder`.
pub struct FakeFolder {
    _temp_dir: tempfile::TempDir,
    local_path: PathBuf,
    remote_path: String,
    local_modifier: DiskFileModifier,
    server: FakeServer,
    account: Arc<Account>,
    journal: Arc<SyncJournalDb>,
    engine: SyncEngine,
    server_version: String,
}

/// `FakeFolder::ErrorList`.
pub struct ErrorList<'a> {
    server: &'a FakeServer,
}

impl ErrorList<'_> {
    /// Upstream default error: 500.
    pub fn append(&self, path: &str, error: u16) {
        self.server
            .state()
            .error_paths
            .insert(path.to_owned(), error);
    }

    pub fn clear(&self) {
        self.server.state().error_paths.clear();
    }
}

/// Guard giving `&mut FileInfo` access to the remote tree
/// (`FakeFolder::remoteModifier()`).
pub struct RemoteGuard<'a>(MutexGuard<'a, ServerState>, bool);

impl std::ops::Deref for RemoteGuard<'_> {
    type Target = FileInfo;
    fn deref(&self) -> &FileInfo {
        if self.1 {
            &self.0.upload_root
        } else {
            &self.0.remote_root
        }
    }
}

impl std::ops::DerefMut for RemoteGuard<'_> {
    fn deref_mut(&mut self) -> &mut FileInfo {
        if self.1 {
            &mut self.0.upload_root
        } else {
            &mut self.0.remote_root
        }
    }
}

/// Runs a future to completion on a fresh current-thread tokio runtime
/// (the event loop of a test).
pub fn run_event_loop<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

impl FakeFolder {
    /// `FakeFolder(fileTemplate)`: performs the initial sync.
    pub fn new(file_template: FileInfo) -> Self {
        Self::with_options(file_template, None, "", true)
    }

    /// `FakeFolder(fileTemplate, localFileInfo, remotePath, performInitialSync)`:
    /// the local directory is seeded from `local_file_info` when given.
    pub fn with_options(
        file_template: FileInfo,
        local_file_info: Option<&FileInfo>,
        remote_path: &str,
        perform_initial_sync: bool,
    ) -> Self {
        let temp_dir = tempfile::tempdir().expect("create temporary directory");
        let local_path = temp_dir
            .path()
            .canonicalize()
            .expect("canonicalize temporary directory");
        to_disk(&local_path, local_file_info.unwrap_or(&file_template));
        let server = FakeServer::new(file_template);
        let server_version = "10.0.0".to_owned();
        let account = Arc::new(Account::new(
            ServerUrl::parse("http://admin:admin@localhost/owncloud").expect("url"),
            Arc::new(server.clone()),
        ));
        account.set_dav_display_name("fakename");
        account.set_server_version(&server_version);
        server.set_server_version(&server_version);
        let local = {
            let mut p = local_path.to_string_lossy().into_owned();
            if !p.ends_with('/') {
                p.push('/');
            }
            p
        };
        let journal = Arc::new(SyncJournalDb::new(format!("{local}.sync_test.db")));
        let mut options = SyncOptions::default();
        // Needs to be done once (SyncEngine::minimumFileAgeForUpload)
        options.minimum_file_age_for_upload = std::time::Duration::ZERO;
        let engine = SyncEngine::new(
            account.clone(),
            &local,
            options,
            remote_path,
            journal.clone(),
        );
        // Ignore temporary files from the download. (This is in the default exclude list, but we don't load it)
        engine.excluded_files().add_manual_exclude(".~lock.*#");
        engine.excluded_files().add_manual_exclude("]*.~*");
        // handle aboutToRemoveAllFiles in case our test does not handle it: continue.
        engine.callbacks().about_to_remove_all_files = Some(Box::new(|_| false));
        let mut ff = Self {
            local_modifier: DiskFileModifier::new(&local_path),
            local_path,
            remote_path: remote_path.to_owned(),
            server,
            account,
            journal,
            engine,
            server_version,
            _temp_dir: temp_dir,
        };
        if perform_initial_sync {
            // A new folder will update the local file state database on first sync.
            // To have a state matching what users will encounter, we have to a sync
            // using an identical local/remote file tree first.
            assert!(ff.sync_once(), "initial sync failed");
        }
        ff
    }

    /// `syncOnce()`.
    pub fn sync_once(&mut self) -> bool {
        let engine = &mut self.engine;
        run_event_loop(engine.sync_once())
    }

    /// `syncOnce()` with `QTimer::singleShot(delay, [&] { syncEngine().abort(); })`.
    pub fn sync_once_aborting_after(&mut self, delay: std::time::Duration) -> bool {
        let abort = self.engine.abort_handle();
        let engine = &mut self.engine;
        run_event_loop(async move {
            let sync = engine.sync_once();
            tokio::pin!(sync);
            let timer = tokio::time::sleep(delay);
            tokio::pin!(timer);
            let mut fired = false;
            loop {
                tokio::select! {
                    r = &mut sync => return r,
                    _ = &mut timer, if !fired => {
                        fired = true;
                        abort.abort();
                    }
                }
            }
        })
    }

    pub fn sync_engine(&mut self) -> &mut SyncEngine {
        &mut self.engine
    }

    pub fn sync_journal(&self) -> &SyncJournalDb {
        &self.journal
    }

    pub fn account(&self) -> &Arc<Account> {
        &self.account
    }

    /// `account()->setCapabilities(...)` from JSON.
    pub fn set_capabilities(&self, caps: serde_json::Value) {
        self.account.set_capabilities(Capabilities::from_json(caps));
    }

    /// The transport to hand to another engine.
    pub fn transport(&self) -> FakeServer {
        self.server.clone()
    }

    pub fn server(&self) -> &FakeServer {
        &self.server
    }

    pub fn local_modifier(&mut self) -> &mut DiskFileModifier {
        &mut self.local_modifier
    }

    /// `remoteModifier()`: mutable access to the remote tree.
    pub fn remote_modifier(&self) -> RemoteGuard<'_> {
        RemoteGuard(self.server.state(), false)
    }

    /// `uploadState()`: the chunked uploads tree.
    pub fn upload_state(&self) -> RemoteGuard<'_> {
        RemoteGuard(self.server.state(), true)
    }

    /// `currentLocalState()`.
    pub fn current_local_state(&self) -> FileInfo {
        from_disk(&self.local_path)
    }

    /// `currentRemoteState()` (a copy).
    pub fn current_remote_state(&self) -> FileInfo {
        self.server.state().remote_root.clone()
    }

    /// `dbState()`: the journal content as a tree.
    pub fn db_state(&self) -> FileInfo {
        let mut result = FileInfo::default();
        let _ = self.journal.get_files_below_path(b"", |record| {
            let components = PathComponents::new(&record.path_str());
            let parent = find_or_create_dirs(&mut result, &components.parent_dir_components());
            let name = components.file_name().to_owned();
            let parent_path = parent.path();
            let item = parent
                .children
                .entry(name.clone())
                .or_insert_with(|| FileInfo::named(&name));
            item.name = name;
            item.parent_path = parent_path;
            item.size = record.file_size;
            item.is_dir = record.item_type == nc_journal::csync::ItemType::Directory;
            item.permissions = record.remote_perm;
            item.etag = String::from_utf8_lossy(&record.etag).into_owned();
            item.last_modified = crate::util::from_secs(record.modtime);
            item.file_id = String::from_utf8_lossy(&record.file_id).into_owned();
            item.checksums = String::from_utf8_lossy(&record.checksum_header).into_owned();
            // item.contentChar can't be set from the db
        });
        result
    }

    /// `serverErrorPaths()`.
    pub fn server_error_paths(&self) -> ErrorList<'_> {
        ErrorList {
            server: &self.server,
        }
    }

    /// `setServerOverride`.
    pub fn set_server_override(
        &self,
        o: impl FnMut(&Request, &mut ServerState) -> Option<FakeReply> + Send + 'static,
    ) {
        self.server.set_override(o);
    }

    /// `setServerVersion`.
    pub fn set_server_version(&mut self, version: &str) {
        if self.server_version == version {
            return;
        }
        self.server_version = version.to_owned();
        self.account.set_server_version(version);
        self.server.set_server_version(version);
    }

    /// `localPath()`: with a trailing slash, as the sync engine wants.
    pub fn local_path(&self) -> String {
        let mut p = self.local_path.to_string_lossy().into_owned();
        if !p.ends_with('/') {
            p.push('/');
        }
        p
    }

    pub fn local_dir(&self) -> &Path {
        &self.local_path
    }

    pub fn remote_path(&self) -> &str {
        &self.remote_path
    }

    /// Where upstream puts the test journal: `localPath() + ".sync_test.db"`.
    pub fn journal_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.sync_test.db", self.local_path()))
    }
}

fn find_or_create_dirs<'a>(
    base: &'a mut FileInfo,
    components: &PathComponents,
) -> &'a mut FileInfo {
    if components.is_empty() {
        return base;
    }
    let child_name = components.path_root().to_owned();
    let base_path = base.path();
    let child = base.children.entry(child_name.clone()).or_insert_with(|| {
        let mut f = FileInfo::named(&child_name);
        f.parent_path = base_path.clone();
        f
    });
    find_or_create_dirs(child, &components.sub_components())
}

/// `ItemCompletedSpy`: records the items of `itemCompleted`.
#[derive(Clone, Default)]
pub struct ItemCompletedSpy {
    items: Rc<RefCell<Vec<SyncFileItemPtr>>>,
}

impl ItemCompletedSpy {
    /// Connects to the engine's `itemCompleted` signal (replacing any
    /// previous connection).
    pub fn new(folder: &mut FakeFolder) -> Self {
        let spy = Self::default();
        let items = spy.items.clone();
        folder.sync_engine().callbacks().item_completed =
            Some(Box::new(move |item: &SyncFileItemPtr, _: ErrorCategory| {
                items.borrow_mut().push(item.clone())
            }));
        spy
    }

    /// `findItem(path)`: the first item whose destination is `path`, or an
    /// empty item.
    pub fn find_item(&self, path: &str) -> SyncFileItem {
        self.items
            .borrow()
            .iter()
            .find(|i| i.borrow().destination() == path)
            .map(|i| i.borrow().clone())
            .unwrap_or_default()
    }

    /// `findItemWithExpectedRank(path, rank)`.
    pub fn find_item_with_expected_rank(&self, path: &str, rank: usize) -> SyncFileItem {
        let items = self.items.borrow();
        assert!(items.len() > rank);
        let item = items[rank].borrow();
        if item.destination() == path {
            item.clone()
        } else {
            SyncFileItem::default()
        }
    }

    pub fn len(&self) -> usize {
        self.items.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.borrow().is_empty()
    }

    pub fn clear(&self) {
        self.items.borrow_mut().clear();
    }

    pub fn items(&self) -> Vec<SyncFileItem> {
        self.items
            .borrow()
            .iter()
            .map(|i| i.borrow().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileModifier;

    #[test]
    fn local_and_remote_start_equal() {
        let mut ff = FakeFolder::new(FileInfo::A12_B12_C12_S12());
        assert_eq!(ff.current_local_state(), ff.current_remote_state());
        ff.local_modifier().insert("A/x", 10, b'L');
        ff.remote_modifier().insert("A/x", 10, b'L');
        assert_eq!(ff.current_local_state(), ff.current_remote_state());
        ff.remote_modifier().append_byte("A/x");
        assert_ne!(ff.current_local_state(), ff.current_remote_state());
        assert!(ff.local_path().ends_with('/'));
        assert!(ff.journal_path().ends_with(".sync_test.db"));
    }

    #[test]
    fn separate_local_template() {
        let local = FileInfo::dir("", [FileInfo::file("only-local", 3)]);
        let ff = FakeFolder::with_options(FileInfo::A12_B12_C12_S12(), Some(&local), "/sub", false);
        assert_eq!(ff.current_local_state(), local);
        assert_eq!(ff.remote_path(), "/sub");
    }
}
