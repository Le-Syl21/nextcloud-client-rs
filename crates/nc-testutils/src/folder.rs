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

//! `FakeFolder`: a local temporary directory plus a [`FakeServer`], both
//! seeded from the same template tree.

use std::path::{Path, PathBuf};
use std::sync::MutexGuard;

use crate::disk::{DiskFileModifier, from_disk, to_disk};
use crate::file_info::FileInfo;
use crate::server::{FakeReply, FakeServer, ServerState};
use nc_dav::Request;

/// Partial port of upstream `FakeFolder`: everything but the sync engine,
/// account and journal, which arrive with `nc-sync` (Phase 1). In
/// particular there is no `sync_once()` yet and no initial sync is run.
pub struct FakeFolder {
    _temp_dir: tempfile::TempDir,
    local_path: PathBuf,
    remote_path: String,
    local_modifier: DiskFileModifier,
    server: FakeServer,
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

impl FakeFolder {
    /// `FakeFolder(fileTemplate)`.
    pub fn new(file_template: FileInfo) -> Self {
        Self::with_options(file_template, None, "")
    }

    /// `FakeFolder(fileTemplate, localFileInfo, remotePath)`: the local
    /// directory is seeded from `local_file_info` when given.
    pub fn with_options(
        file_template: FileInfo,
        local_file_info: Option<&FileInfo>,
        remote_path: &str,
    ) -> Self {
        let temp_dir = tempfile::tempdir().expect("create temporary directory");
        let local_path = temp_dir
            .path()
            .canonicalize()
            .expect("canonicalize temporary directory");
        to_disk(&local_path, local_file_info.unwrap_or(&file_template));
        Self {
            local_modifier: DiskFileModifier::new(&local_path),
            local_path,
            remote_path: remote_path.to_owned(),
            server: FakeServer::new(file_template),
            _temp_dir: temp_dir,
        }
    }

    /// The transport to hand to the sync engine.
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
    pub fn set_server_version(&self, version: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileModifier, block_on};
    use nc_dav::Transport;

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

        ff.server_error_paths().append("A/a1", 500);
        let r = http::Request::builder()
            .uri(format!("{}A/a1", crate::server::ROOT_URL2))
            .body(nc_dav::Body::empty())
            .unwrap();
        assert_eq!(block_on(ff.transport().send(r)).unwrap().status(), 500);
        ff.server_error_paths().clear();
    }

    #[test]
    fn separate_local_template() {
        let local = FileInfo::dir("", [FileInfo::file("only-local", 3)]);
        let ff = FakeFolder::with_options(FileInfo::A12_B12_C12_S12(), Some(&local), "/sub");
        assert_eq!(ff.current_local_state(), local);
        assert_eq!(ff.remote_path(), "/sub");
    }
}
