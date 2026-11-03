// SPDX-FileCopyrightText: 2023 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/caseclashconflictsolver.{h,cpp}`
// (nextcloud/desktop v34.0.5).

//! `CaseClashConflictSolver`: renames the server's copy of a file that
//! clashes (differs from another file only in case) on a case-preserving file
//! system, so that both can be downloaded. The GUI's "solve conflict" dialog
//! drives it: it asks [`CaseClashConflictSolver::check_if_allowed_to_rename`]
//! once, then [`CaseClashConflictSolver::solve_conflict`] with the name the
//! user picked. A clash is never dropped silently: until it is solved the
//! local conflict copy and its journal record stay, and the sync reports it.
//!
//! Upstream's signals are the return values: `done()` is `Ok(())`,
//! `failed()` an [`SolverError`], `errorStringChanged` the message of the
//! error.

use nc_dav::account::Account;
use nc_dav::jobs::{JobOptions, Target, move_, propfind};
use nc_journal::journal::SyncJournalDb;
use nc_journal::remote_permissions::{Permission, RemotePermissions};

const LOG: &str = "nextcloud.sync.caseclash.solver";

/// Why the clash could not be solved; the message is what the dialog shows.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SolverError(pub String);

/// The answer of [`CaseClashConflictSolver::check_if_allowed_to_rename`]
/// (`allowedToRename` and `errorString`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameCheck {
    pub allowed_to_rename: bool,
    pub error_string: String,
}

/// `CaseClashConflictSolver`.
pub struct CaseClashConflictSolver<'a> {
    account: &'a Account,
    journal: &'a SyncJournalDb,
    /// The local path of the file that was downloaded (the "target").
    target_file_path: String,
    /// The local conflict copy (`... (case clash from ...)`).
    conflict_file_path: String,
    remote_path: String,
    local_path: String,
}

impl<'a> CaseClashConflictSolver<'a> {
    pub fn new(
        target_file_path: &str,
        conflict_file_path: &str,
        remote_path: &str,
        local_path: &str,
        account: &'a Account,
        journal: &'a SyncJournalDb,
    ) -> Self {
        Self {
            account,
            journal,
            target_file_path: target_file_path.to_owned(),
            conflict_file_path: conflict_file_path.to_owned(),
            remote_path: remote_path.to_owned(),
            local_path: local_path.to_owned(),
        }
    }

    /// `remoteNewFilename()`.
    fn remote_new_filename(&self, new_filename: &str) -> String {
        if self.remote_path == "/" {
            new_filename.to_owned()
        } else {
            format!("{}{new_filename}", self.remote_path)
        }
    }

    /// `remoteTargetFilePath()`.
    fn remote_target_file_path(&self) -> String {
        let relative = self
            .target_file_path
            .get(self.local_path.len()..)
            .unwrap_or_default();
        if self.remote_path == "/" {
            relative.to_owned()
        } else {
            format!("{}{relative}", self.remote_path)
        }
    }

    /// `solveConflict(newFilename)`: the new name must be free on the server;
    /// the clashing file is moved there, then the conflict record and the
    /// local conflict copy go away.
    pub async fn solve_conflict(&self, new_filename: &str) -> Result<(), SolverError> {
        let opts = JobOptions::default();
        let destination = nc_platform::paths::clean_path(&self.remote_new_filename(new_filename));
        if propfind(self.account, &destination, &["getlastmodified"], &opts)
            .await
            .is_ok()
        {
            // onRemoteDestinationFileAlreadyExists
            return Err(SolverError("Cannot rename file because a file with the same name already exists on the server. Please pick another name.".to_owned()));
        }
        // onRemoteDestinationFileDoesNotExist
        let source = nc_platform::paths::clean_path(&self.remote_target_file_path());
        if propfind(self.account, &source, &["getlastmodified"], &opts)
            .await
            .is_err()
        {
            // onRemoteSourceFileDoesNotExist: failed()
            return Err(SolverError(String::new()));
        }
        // onRemoteSourceFileAlreadyExists
        let remote_destination = nc_platform::paths::clean_path(&format!(
            "{}{}",
            self.account.dav_url_path(),
            self.remote_new_filename(new_filename)
        ));
        log::info!(target: LOG, "rename case clashing file from {source} to {remote_destination}");
        let reply = move_(
            self.account,
            &Target::Dav(source),
            &remote_destination,
            &[],
            &opts,
        )
        .await;
        if !(200..300).contains(&reply.http_status) {
            // onMoveJobFinished
            return Err(SolverError(
                "Could not rename file. Please make sure you are connected to the server."
                    .to_owned(),
            ));
        }
        log::info!(target: LOG, "going to delete case clash conflict record {}", self.target_file_path);
        self.journal
            .delete_case_clash_conflict_by_path_record(&self.target_file_path);
        log::info!(target: LOG, "going to delete {}", self.conflict_file_path);
        let _ = crate::filesystem::remove(&self.conflict_file_path);
        Ok(())
    }

    /// `checkIfAllowedToRename()` and `onCheckIfAllowedToRenameComplete`.
    pub async fn check_if_allowed_to_rename(&self, new_filename: &str) -> RenameCheck {
        const CONTENT_NOT_FOUND_ERROR: u16 = 404;
        let path = nc_platform::paths::clean_path(&self.remote_target_file_path());
        let result = propfind(
            self.account,
            &path,
            &[
                "http://owncloud.org/ns:permissions",
                "http://nextcloud.org/ns:is-mount-root",
            ],
            &JobOptions::default(),
        )
        .await;
        match result {
            Ok(values) => {
                if let Some(p) = values.get("permissions") {
                    let perms = RemotePermissions::from_server_string(p);
                    if !(perms.has_permission(Permission::CanRename)
                        && perms.has_permission(Permission::CanMove))
                    {
                        return not_allowed("You don't have the permission to rename this file. Please ask the author of the file to rename it.".to_owned());
                    }
                }
            }
            Err(reply) if reply.http_status != CONTENT_NOT_FOUND_ERROR => {
                return not_allowed(format!(
                    "Failed to fetch permissions with error {}",
                    reply.http_status
                ));
            }
            Err(_) => {}
        }
        process_leading_or_trailing_spaces_error(file_name_of(new_filename))
    }
}

fn not_allowed(error_string: String) -> RenameCheck {
    RenameCheck {
        allowed_to_rename: false,
        error_string,
    }
}

fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `processLeadingOrTrailingSpacesError`.
pub fn process_leading_or_trailing_spaces_error(file_name: &str) -> RenameCheck {
    let leading = file_name.starts_with(' ');
    let trailing = file_name.ends_with(' ');
    let message = match (leading, trailing) {
        (true, true) => "Filename contains leading and trailing spaces.",
        (true, false) => "Filename contains leading spaces.",
        (false, true) => "Filename contains trailing spaces.",
        (false, false) => {
            return RenameCheck {
                allowed_to_rename: true,
                error_string: String::new(),
            };
        }
    };
    not_allowed(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_are_refused_with_their_message() {
        assert!(process_leading_or_trailing_spaces_error("a b").allowed_to_rename);
        let r = process_leading_or_trailing_spaces_error(" a");
        assert!(!r.allowed_to_rename);
        assert_eq!(r.error_string, "Filename contains leading spaces.");
        assert_eq!(
            process_leading_or_trailing_spaces_error(" a ").error_string,
            "Filename contains leading and trailing spaces."
        );
        assert_eq!(
            process_leading_or_trailing_spaces_error("a ").error_string,
            "Filename contains trailing spaces."
        );
    }
}
