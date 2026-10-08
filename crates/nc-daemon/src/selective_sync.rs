// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the journal part of upstream `FolderStatusModel::slotApplySelectiveSync`
// (`src/gui/folderstatusmodel.cpp`) and of the path handling of
// `Folder::blacklistPath` / `appendPathToSelectiveSyncList`
// (`src/gui/folder.cpp`) (nextcloud/desktop v34.0.5).

//! Selective sync ("Choose what to sync") edits of a folder's journal:
//! `ncsync folder exclude|include`.
//!
//! Upstream builds the new blacklist from the checked state of a tree of the
//! remote folders (`createBlackList`): an unchecked folder is listed, its
//! children are not. Here the list is edited one path at a time: excluding a
//! folder drops the entries below it, including a folder removes its entry.
//! A folder inside an excluded folder cannot be included on its own (the
//! tree would list all its siblings instead, which needs the server).
//!
//! Then, like `slotApplySelectiveSync`: the folders that were blacklisted or
//! undecided and no longer are go to the white list, the undecided list is
//! cleared, and every changed path is scheduled for remote discovery (its
//! etag and those of its parents are invalidated). The folder also schedules
//! it for local discovery and syncs at once. The next sync downloads the
//! re-included folders; for a newly excluded folder, the discovery removes
//! the local files that are unchanged since the last sync and keeps (and
//! ignores) the others (`ProcessDirectoryJob::processBlacklisted`).

use std::collections::BTreeSet;

use nc_journal::journal::{SelectiveSyncListType, SyncJournalDb, find_path_in_selective_sync_list};

const LOG: &str = "nextcloud.gui.folder";

/// Errors of a selective sync edit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectiveSyncError {
    #[error(
        "invalid folder path {0:?} (a path relative to the folder's remote root, e.g. Photos/2020)"
    )]
    InvalidPath(String),
    #[error("{0} is a file: only folders can be excluded")]
    NotAFolder(String),
    #[error(
        "{path} is inside the excluded folder {excluded}: include {excluded} (then exclude its other subfolders)"
    )]
    InsideExcluded { path: String, excluded: String },
    #[error("Could not read selective sync list from db.")]
    ReadFailed,
}

/// What an edit did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectiveSyncChange {
    /// The blacklist after the edit (sorted).
    pub black_list: Vec<String>,
    /// The paths whose state changed (excluded or included), sorted.
    pub changes: Vec<String>,
    /// The paths added to the white list.
    pub white_list_added: Vec<String>,
}

/// `QString::operator<` order (UTF-16 code units), the order
/// `findPathInSelectiveSyncList` expects.
fn sort_qstring(list: &mut [String]) {
    list.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

/// Normalizes a folder path of the selective sync lists: relative to the
/// folder's remote root, without `.`/`..` segments, ending with `/`
/// (`Utility::trailingSlashPath`).
pub fn normalize_path(path: &str) -> Result<String, SelectiveSyncError> {
    let trimmed = path.trim();
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() || segments.iter().any(|s| *s == "." || *s == "..") {
        return Err(SelectiveSyncError::InvalidPath(path.to_owned()));
    }
    Ok(nc_journal::utility::trailing_slash_path(
        &segments.join("/"),
    ))
}

/// The excluded folder (of a sorted list) containing `path` (with its
/// trailing slash), the path itself included.
fn excluded_by(list: &[String], path: &str) -> Option<String> {
    let bare = path.trim_end_matches('/');
    if !find_path_in_selective_sync_list(list, bare) {
        return None;
    }
    list.iter()
        .find(|e| path.starts_with(e.as_str()) || e.as_str() == "/")
        .cloned()
}

/// The new blacklist: `old` with the (normalized) `exclude` paths added and
/// the `include` paths removed, in that order.
pub fn edit_black_list(
    old: &[String],
    exclude: &[String],
    include: &[String],
) -> Result<Vec<String>, SelectiveSyncError> {
    let mut list: Vec<String> = old.to_vec();
    sort_qstring(&mut list);
    for path in exclude {
        if excluded_by(&list, path).is_some() {
            continue; // already excluded, by itself or by a parent
        }
        // createBlackList lists an unchecked folder, not its children.
        list.retain(|e| !e.starts_with(path.as_str()));
        list.push(path.clone());
        sort_qstring(&mut list);
    }
    for path in include {
        if list.iter().any(|e| e == path) {
            list.retain(|e| e != path);
            // Folders below it excluded before were dropped with the
            // exclusion; nothing else to do.
            continue;
        }
        if let Some(excluded) = excluded_by(&list, path) {
            return Err(SelectiveSyncError::InsideExcluded {
                path: path.clone(),
                excluded,
            });
        }
    }
    Ok(list)
}

/// The current blacklist of a journal.
pub fn black_list(journal: &SyncJournalDb) -> Result<Vec<String>, SelectiveSyncError> {
    let mut list = journal
        .get_selective_sync_list(SelectiveSyncListType::BlackList)
        .map_err(|_| {
            log::warn!(target: LOG, "Could not read selective sync list from db.");
            SelectiveSyncError::ReadFailed
        })?;
    sort_qstring(&mut list);
    Ok(list)
}

/// Excludes and includes folders in the journal's selective sync lists
/// (see the module documentation). Nothing is written when both lists are
/// empty.
pub fn apply(
    journal: &SyncJournalDb,
    exclude: &[String],
    include: &[String],
) -> Result<SelectiveSyncChange, SelectiveSyncError> {
    let exclude = exclude
        .iter()
        .map(|p| normalize_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    let include = include
        .iter()
        .map(|p| normalize_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    let old_black_list = black_list(journal)?;
    if exclude.is_empty() && include.is_empty() {
        return Ok(SelectiveSyncChange {
            black_list: old_black_list,
            ..SelectiveSyncChange::default()
        });
    }
    for path in &exclude {
        let bare = path.trim_end_matches('/');
        if let Ok(Some(record)) = journal.get_file_record(bare.as_bytes())
            && record.is_valid()
            && !record.is_directory()
        {
            return Err(SelectiveSyncError::NotAFolder(bare.to_owned()));
        }
    }
    let black_list = edit_black_list(&old_black_list, &exclude, &include)?;
    journal.set_selective_sync_list(SelectiveSyncListType::BlackList, &black_list);

    let black_list_set: BTreeSet<&String> = black_list.iter().collect();
    let old_black_list_set: BTreeSet<&String> = old_black_list.iter().collect();

    // The folders that were undecided or blacklisted and that are now checked should go on the white list.
    // The user confirmed them already just now.
    let undecided = journal
        .get_selective_sync_list(SelectiveSyncListType::UndecidedList)
        .unwrap_or_default();
    let to_add_to_white_list: Vec<String> = old_black_list
        .iter()
        .chain(undecided.iter())
        .filter(|p| !black_list_set.contains(p))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect();
    if !to_add_to_white_list.is_empty()
        && let Ok(mut white_list) =
            journal.get_selective_sync_list(SelectiveSyncListType::WhiteList)
    {
        white_list.extend(to_add_to_white_list.iter().cloned());
        journal.set_selective_sync_list(SelectiveSyncListType::WhiteList, &white_list);
    }
    // clear the undecided list
    journal.set_selective_sync_list(SelectiveSyncListType::UndecidedList, &[]);

    let mut changes: Vec<String> = old_black_list_set
        .symmetric_difference(&black_list_set)
        .map(|p| (*p).clone())
        .collect();
    sort_qstring(&mut changes);
    // The part that changed should not be read from the DB on next sync because there might be new folders
    // (the ones that are no longer in the blacklist)
    for it in &changes {
        journal.schedule_path_for_remote_discovery(it.as_bytes());
    }
    Ok(SelectiveSyncChange {
        black_list,
        changes,
        white_list_added: to_add_to_white_list,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn derived_normalize_path() {
        assert_eq!(normalize_path("Photos").unwrap(), "Photos/");
        assert_eq!(normalize_path("/Photos/2020/").unwrap(), "Photos/2020/");
        assert_eq!(normalize_path(" a//b ").unwrap(), "a/b/");
        for bad in ["", "/", "  ", "a/../b", "./a"] {
            assert!(normalize_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn derived_edit_black_list() {
        // exclude: children entries are dropped, a parent covers its children
        assert_eq!(
            edit_black_list(&v(&["A/b/", "C/"]), &v(&["A/"]), &[]).unwrap(),
            v(&["A/", "C/"])
        );
        assert_eq!(
            edit_black_list(&v(&["A/"]), &v(&["A/b/"]), &[]).unwrap(),
            v(&["A/"])
        );
        // a sibling with a common prefix is not a child
        assert_eq!(
            edit_black_list(&v(&["AB/"]), &v(&["A/"]), &[]).unwrap(),
            v(&["A/", "AB/"])
        );
        // include
        assert_eq!(
            edit_black_list(&v(&["A/", "C/"]), &[], &v(&["A/"])).unwrap(),
            v(&["C/"])
        );
        assert_eq!(
            edit_black_list(&v(&["A/"]), &[], &v(&["B/"])).unwrap(),
            v(&["A/"])
        );
        assert_eq!(
            edit_black_list(&v(&["A/"]), &[], &v(&["A/b/"])),
            Err(SelectiveSyncError::InsideExcluded {
                path: "A/b/".into(),
                excluded: "A/".into()
            })
        );
    }

    #[test]
    fn derived_apply_updates_the_lists_like_slot_apply_selective_sync() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SyncJournalDb::new(dir.path().join(".sync_test.db"));
        journal.set_selective_sync_list(SelectiveSyncListType::BlackList, &v(&["Big/"]));
        journal.set_selective_sync_list(SelectiveSyncListType::UndecidedList, &v(&["Big/"]));
        // listing writes nothing
        let r = apply(&journal, &[], &[]).unwrap();
        assert_eq!(r.black_list, v(&["Big/"]));
        assert!(r.changes.is_empty());
        assert_eq!(
            journal
                .get_selective_sync_list(SelectiveSyncListType::UndecidedList)
                .unwrap(),
            v(&["Big/"])
        );
        // exclude one, include the undecided one
        let r = apply(&journal, &v(&["/Photos/"]), &v(&["Big"])).unwrap();
        assert_eq!(r.black_list, v(&["Photos/"]));
        assert_eq!(r.changes, v(&["Big/", "Photos/"]));
        assert_eq!(r.white_list_added, v(&["Big/"]));
        let get = |t| journal.get_selective_sync_list(t).unwrap();
        assert_eq!(get(SelectiveSyncListType::BlackList), v(&["Photos/"]));
        assert_eq!(get(SelectiveSyncListType::WhiteList), v(&["Big/"]));
        assert!(get(SelectiveSyncListType::UndecidedList).is_empty());
        assert!(apply(&journal, &v(&[".."]), &[]).is_err());
    }
}
