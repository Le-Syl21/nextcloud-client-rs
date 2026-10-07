// SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/propagatorjobs.{h,cpp}` (PropagateLocalRemove,
// PropagateLocalMkdir, PropagateLocalRename) and
// `PropagateRemoteMove::adjustSelectiveSync` of nextcloud/desktop v34.0.5.

//! The jobs that act on the local file system only. They run synchronously,
//! like upstream where `start()` calls `done()` before returning.

use nc_journal::journal::{SelectiveSyncListType, SyncJournalDb};

use super::{Done, JobCtx, LOG, Outcome, PropagatorEvent};
use crate::filesystem::{self, FilePermissionsRestore, FolderPermissions};
use crate::item::{ErrorCategory, Instruction, LockStatus, Status, SyncFileItem};

fn generic(status: Status, msg: impl Into<String>) -> Outcome {
    Some(Done::new(status, msg, ErrorCategory::GenericError))
}

/// `isPathInsideDeletedDir(path, deletedDir)`: whether `path` sits inside the
/// already removed directory `deleted_dir`. Compares with a trailing slash so
/// a sibling such as "A/BC" is not mistaken for a child of "A/B".
pub fn is_path_inside_deleted_dir(path: &str, deleted_dir: &str) -> bool {
    !deleted_dir.is_empty()
        && path.len() > deleted_dir.len()
        && path.as_bytes()[deleted_dir.len()] == b'/'
        && path.starts_with(deleted_dir)
}

/// `journalRelativePath(syncRoot, filesystemPath)`.
fn journal_relative_path(sync_root: &str, filesystem_path: &str) -> Option<String> {
    let root = sync_root.trim_end_matches('/');
    let rel = filesystem_path.strip_prefix(root)?;
    let rel = rel.trim_start_matches('/');
    if rel == ".." || rel.starts_with("../") {
        return None;
    }
    Some(rel.trim_end_matches('/').to_owned())
}

/// `PropagateLocalRemove::removeRecursively(path)`: on failure, the journal
/// entries of what was deleted are removed.
fn remove_recursively(ctx: &JobCtx, file: &str, trash: &[String]) -> bool {
    let absolute = ctx.shared.full_local_path(file);
    let _parent = FilePermissionsRestore::new(
        &filesystem::parent_dir(&absolute),
        FolderPermissions::ReadWrite,
    );
    ctx.shared
        .emit(PropagatorEvent::TouchedFile(absolute.clone()));
    let root = ctx.shared.local_dir.clone();
    let mut deleted: Vec<(String, bool)> = Vec::new();
    let mut conversion_error = false;
    let mut errors = Vec::new();
    // Entries to move to the client trash bin are moved instead of deleted.
    let success = if trash.is_empty() {
        filesystem::remove_recursively(
            &absolute,
            &mut |p, is_dir| match journal_relative_path(&root, p) {
                // by prepending, a folder deletion may be followed by content deletions
                Some(rel) => deleted.insert(0, (rel, is_dir)),
                None => conversion_error = true,
            },
            &mut errors,
        )
    } else {
        for t in trash {
            if filesystem::file_exists(t) && trash::delete(t).is_err() {
                let _ = filesystem::remove(t);
            }
        }
        filesystem::remove_recursively(
            &absolute,
            &mut |p, is_dir| match journal_relative_path(&root, p) {
                Some(rel) => deleted.insert(0, (rel, is_dir)),
                None => conversion_error = true,
            },
            &mut errors,
        )
    };
    if !success {
        // We need to delete the entries from the database now from the deleted vector.
        // Do it while avoiding redundant delete calls to the journal.
        let mut deleted_dir = String::new();
        for (path, is_dir) in &deleted {
            if is_path_inside_deleted_dir(path, &deleted_dir) {
                continue;
            }
            if ctx.shared.journal.delete_file_record(path, *is_dir).is_ok() {
                if *is_dir {
                    deleted_dir = path.clone();
                }
            } else {
                log::warn!(target: "nextcloud.sync.propagator.localremove", "Failed to delete file record from local DB {path}");
            }
        }
    }
    if conversion_error {
        log::warn!(target: "nextcloud.sync.propagator.localremove", "Failed to convert a deleted filesystem path to a journal path");
        return false;
    }
    success
}

/// `PropagateLocalRemove::start`.
pub(super) fn local_remove(ctx: &JobCtx, trash: &[String]) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let (file, original_file, is_dir) = {
        let i = ctx.item.borrow();
        (i.file.clone(), i.original_file.clone(), i.is_directory())
    };
    let filename = ctx.shared.full_local_path(&file);
    if ctx.shared.local_file_name_clash(&file) {
        return generic(
            Status::FileNameClash,
            format!("Could not remove {filename} because of a local file name clash"),
        );
    }
    // Moving to the trash is only feasible with the Windows cfapi VFS.
    if is_dir {
        if filesystem::file_exists(&filename) && !remove_recursively(ctx, &file, trash) {
            return generic(
                Status::NormalError,
                "Temporary error when removing local item removed from server.",
            );
        }
    } else if filesystem::file_exists(&filename) {
        let _parent = FilePermissionsRestore::new(
            &filesystem::parent_dir(&filename),
            FolderPermissions::ReadWrite,
        );
        if let Err(e) = filesystem::remove(&filename) {
            log::warn!(target: "nextcloud.sync.propagator.localremove", "remove failed {filename} {e}");
            return generic(
                Status::NormalError,
                "Temporary error when removing local item removed from server.",
            );
        }
    }
    if ctx
        .shared
        .journal
        .delete_file_record(&original_file, is_dir)
        .is_err()
    {
        log::warn!(target: "nextcloud.sync.propagator.localremove", "could not delete file from local DB {original_file}");
        return generic(
            Status::NormalError,
            format!("Could not delete file record {original_file} from local DB"),
        );
    }
    ctx.shared.journal.commit("Local remove", true);
    Some(Done::ok())
}

/// `PropagateLocalMkdir::start` / `startLocalMkdir`.
pub(super) fn local_mkdir(ctx: &JobCtx) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let (file, instruction, remote_perm) = {
        let i = ctx.item.borrow();
        (i.file.clone(), i.instruction, i.remote_perm)
    };
    let new_dir_str = ctx.shared.full_local_path(&file);
    // When turning something that used to be a file into a directory
    // we need to delete the file first.
    if filesystem::file_exists(&new_dir_str) && filesystem::is_file(&new_dir_str) {
        if ctx.delete_existing {
            if let Err(e) = filesystem::remove(&new_dir_str) {
                return generic(
                    Status::NormalError,
                    format!("could not delete file {new_dir_str}, error: {e}"),
                );
            }
        } else if instruction == Instruction::Conflict
            && let Err(error) = ctx
                .shared
                .create_conflict(&ctx.item, ctx.associated_composite)
        {
            return generic(Status::SoftError, error);
        }
    }
    if nc_journal::utility::fs_case_preserving() && ctx.shared.local_file_name_clash(&file) {
        log::warn!(target: "nextcloud.sync.propagator.localmkdir", "New folder to create locally already exists with different case: {file}");
        return generic(
            Status::FileNameClash,
            format!(
                "Folder {new_dir_str} cannot be created because of a local file or folder name clash!"
            ),
        );
    }
    let parent_folder_path = filesystem::parent_dir(&new_dir_str);
    let mut parent_need_rollback_permissions = false;
    if filesystem::is_folder_read_only(&parent_folder_path) {
        filesystem::set_folder_permissions(&parent_folder_path, FolderPermissions::ReadWrite);
        parent_need_rollback_permissions = true;
        ctx.shared
            .emit(PropagatorEvent::TouchedFile(parent_folder_path.clone()));
    }
    ctx.shared
        .emit(PropagatorEvent::TouchedFile(new_dir_str.clone()));
    if !filesystem::mkpath(&ctx.shared.full_local_path(&file)) {
        return generic(
            Status::NormalError,
            format!("Could not create folder {new_dir_str}"),
        );
    }
    if !remote_perm.is_null()
        && !remote_perm.has_permissions_for_read_write()
        && !filesystem::set_folder_permissions(&new_dir_str, FolderPermissions::ReadOnly)
    {
        return generic(
            Status::NormalError,
            format!("The folder {file} cannot be made read-only: permission change failed"),
        );
    }
    if parent_need_rollback_permissions {
        filesystem::set_folder_permissions(&parent_folder_path, FolderPermissions::ReadOnly);
        ctx.shared
            .emit(PropagatorEvent::TouchedFile(parent_folder_path));
    }
    // Insert the directory into the database. The correct etag will be set later,
    // once all contents have been propagated, because should_update_metadata is true.
    // Adding an entry with a dummy etag to the database still makes sense here
    // so the database is aware that this folder exists even if the sync is aborted
    // before the correct etag is stored.
    let mut new_item: SyncFileItem = ctx.item.borrow().clone();
    new_item.etag = b"_invalid_".to_vec();
    if let Err(e) = ctx.shared.update_metadata(&new_item) {
        return generic(Status::FatalError, format!("Error updating metadata: {e}"));
    }
    ctx.shared.journal.commit("localMkdir", true);
    let result_status = if instruction == Instruction::Conflict {
        Status::Conflict
    } else {
        Status::Success
    };
    Some(Done::new(result_status, "", ErrorCategory::NoError))
}

/// `PropagateRemoteMove::adjustSelectiveSync`: keeps the blacklist pointing
/// at a renamed folder.
pub(crate) fn adjust_selective_sync(journal: &SyncJournalDb, from: &str, to: &str) -> bool {
    let Ok(mut list) = journal.get_selective_sync_list(SelectiveSyncListType::BlackList) else {
        return false;
    };
    let from = format!("{from}/");
    let to = format!("{to}/");
    let mut changed = false;
    for s in list.iter_mut() {
        if s.starts_with(&from) {
            *s = format!("{to}{}", &s[from.len()..]);
            changed = true;
        }
    }
    if changed {
        journal.set_selective_sync_list(SelectiveSyncListType::BlackList, &list);
    }
    true
}

/// `PropagateLocalRename::deleteOldDbRecord`.
fn delete_old_db_record(ctx: &JobCtx, file_name: &str) -> Result<(), Done> {
    if ctx
        .shared
        .journal
        .get_file_record(file_name.as_bytes())
        .is_err()
    {
        log::warn!(target: "nextcloud.sync.propagator.localrename", "Could not get file from local DB {file_name}");
        return Err(Done::new(
            Status::NormalError,
            format!("Could not get file {file_name} from local DB"),
            ErrorCategory::GenericError,
        ));
    }
    if ctx
        .shared
        .journal
        .delete_file_record(file_name, false)
        .is_err()
    {
        log::warn!(target: "nextcloud.sync.propagator.localrename", "could not delete file from local DB {file_name}");
        return Err(Done::new(
            Status::NormalError,
            format!("Could not delete file record {file_name} from local DB"),
            ErrorCategory::GenericError,
        ));
    }
    Ok(())
}

/// `PropagateLocalRename::start`.
pub(super) fn local_rename(ctx: &JobCtx) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let (file, rename_target, original_file, is_dir) = {
        let i = ctx.item.borrow();
        (
            i.file.clone(),
            i.rename_target.clone(),
            i.original_file.clone(),
            i.is_directory(),
        )
    };
    let previous_name_in_db = ctx.shared.adjust_renamed_path(&file);
    let existing_file = ctx.shared.full_local_path(&previous_name_in_db);
    let target_file = ctx.shared.full_local_path(&rename_target);
    let original_full = ctx.shared.full_local_path(&original_file);
    let orig_exists = filesystem::file_exists(&original_full);
    let existing_exists = filesystem::file_exists(&existing_file);
    let target_exists = filesystem::file_exists(&target_file);
    let file_already_moved = (!orig_exists || !existing_exists) && target_exists;

    // if the file is a file underneath a moved dir, the _item->file is equal
    // to _item->renameTarget and the file is not moved as a result.
    if file != rename_target {
        if file.to_lowercase() != rename_target.to_lowercase()
            && ctx.shared.local_file_name_clash(&rename_target)
        {
            if is_dir {
                return generic(
                    Status::FileNameClash,
                    format!(
                        "Folder {file} cannot be renamed because of a local file or folder name clash!"
                    ),
                );
            }
            return match ctx
                .shared
                .create_case_clash_conflict(&ctx.item, &existing_file)
            {
                Some(e) => generic(Status::SoftError, e),
                None => generic(
                    Status::FileNameClash,
                    format!("File {file} downloaded but it resulted in a local file name clash!"),
                ),
            };
        }
        let target_parent = filesystem::parent_dir(&target_file);
        let target_parent_was_read_only = filesystem::is_folder_read_only(&target_parent);
        if target_parent_was_read_only {
            filesystem::set_folder_permissions(&target_parent, FolderPermissions::ReadWrite);
            ctx.shared
                .emit(PropagatorEvent::TouchedFile(target_parent.clone()));
        }
        let origin_parent = filesystem::parent_dir(&existing_file);
        let origin_parent_was_read_only = filesystem::is_folder_read_only(&origin_parent);
        if origin_parent_was_read_only {
            filesystem::set_folder_permissions(&origin_parent, FolderPermissions::ReadWrite);
            ctx.shared
                .emit(PropagatorEvent::TouchedFile(origin_parent.clone()));
        }
        let restore = |p: &str| {
            filesystem::set_folder_permissions(p, FolderPermissions::ReadOnly);
            ctx.shared.emit(PropagatorEvent::TouchedFile(p.to_owned()));
        };
        let rename_result = {
            let _folder_permissions =
                FilePermissionsRestore::new(&existing_file, FolderPermissions::ReadWrite);
            ctx.shared
                .emit(PropagatorEvent::TouchedFile(existing_file.clone()));
            ctx.shared
                .emit(PropagatorEvent::TouchedFile(target_file.clone()));
            filesystem::rename(&existing_file, &target_file)
        };
        if target_parent_was_read_only {
            restore(&target_parent);
        }
        if origin_parent_was_read_only {
            restore(&origin_parent);
        }
        if let Err(rename_error) = rename_result {
            return generic(Status::NormalError, rename_error);
        }
    }

    let old_record = match ctx.shared.journal.get_file_record(if file_already_moved {
        previous_name_in_db.as_bytes()
    } else {
        original_file.as_bytes()
    }) {
        Ok(r) => r.unwrap_or_default(),
        Err(_) => {
            log::warn!(target: "nextcloud.sync.propagator.localrename", "Could not get file from local DB {original_file}");
            return generic(
                Status::NormalError,
                format!("Could not get file {original_file} from local DB"),
            );
        }
    };
    if let Err(d) = delete_old_db_record(ctx, &previous_name_in_db) {
        return Some(d);
    }

    if !is_dir {
        // Directories are saved at the end
        let mut new_item: SyncFileItem = ctx.item.borrow().clone();
        if old_record.is_valid() {
            new_item.checksum_header = old_record.checksum_header.clone();
        }
        if let Err(e) = ctx.shared.update_metadata(&new_item) {
            return generic(Status::FatalError, format!("Error updating metadata: {e}"));
        }
    } else if !file_already_moved {
        let mut below = Vec::new();
        if ctx
            .shared
            .journal
            .get_files_below_path(previous_name_in_db.as_bytes(), |r| below.push(r.clone()))
            .is_err()
        {
            return generic(
                Status::FatalError,
                "Failed to propagate directory rename in hierarchy",
            );
        }
        for record in below {
            let old_file_name = ctx.shared.adjust_renamed_path(&record.path_str());
            let new_file_name = if old_file_name.len() >= previous_name_in_db.len() {
                format!(
                    "{}{}",
                    rename_target,
                    &old_file_name[previous_name_in_db.len()..]
                )
            } else {
                old_file_name.clone()
            };
            if old_file_name == new_file_name {
                continue;
            }
            let Ok(Some(old)) = ctx.shared.journal.get_file_record(old_file_name.as_bytes()) else {
                log::warn!(target: "nextcloud.sync.propagator.localrename", "Could not get file from local DB {old_file_name}");
                continue;
            };
            if ctx
                .shared
                .journal
                .delete_file_record(&old_file_name, false)
                .is_err()
            {
                log::warn!(target: "nextcloud.sync.propagator.localrename", "could not delete file from local DB {old_file_name}");
                continue;
            }
            let mut new_item = SyncFileItem::from_sync_journal_file_record(&old);
            new_item.file = new_file_name;
            new_item.lock_token.clear();
            new_item.locked = LockStatus::UnlockedItem;
            if ctx.shared.update_metadata(&new_item).is_err() {
                continue;
            }
        }
        ctx.shared
            .renamed_directories
            .borrow_mut()
            .insert(file.clone(), rename_target.clone());
        if !adjust_selective_sync(&ctx.shared.journal, &file, &rename_target) {
            return generic(Status::FatalError, "Failed to rename file");
        }
    }
    ctx.shared.journal.commit("localRename", true);
    log::debug!(target: LOG, "local rename done {file} -> {rename_target}");
    Some(Done::ok())
}
