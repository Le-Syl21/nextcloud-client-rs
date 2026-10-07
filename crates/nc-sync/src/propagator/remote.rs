// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/propagateremotedelete.cpp`,
// `propagateremotemkdir.cpp` and `propagateremotemove.cpp`
// (nextcloud/desktop v34.0.5), without the end-to-end encryption paths.

//! The jobs that act on the server: DELETE, MKCOL (+ PROPFIND of the new
//! folder's permissions) and MOVE.

use nc_dav::{JobOptions, NetworkError, Target};
use nc_journal::remote_permissions::{MountedPermissionAlgorithm, Permission, RemotePermissions};

use super::{Done, JobCtx, Outcome, classify_error, error_category_from_network_error};
use crate::filesystem::{self, FilePermissionsRestore, FolderPermissions};
use crate::item::{
    ErrorCategory, LockStatus, Status, SyncFileItem, SynchronizationOptions,
    current_msecs_since_epoch,
};

fn opts(ctx: &JobCtx) -> JobOptions {
    JobOptions::with_cancel(ctx.shared.soft_abort.child_token())
}

fn lock_if_header(ctx: &JobCtx, file: &str, lock_token: &str) -> Vec<u8> {
    format!(
        "<{}{}> (<opaquelocktoken:{}>)",
        ctx.shared.account.dav_url_string(),
        file,
        lock_token
    )
    .into_bytes()
}

/// `PropagateRemoteDelete::start` + `slotDeleteJobFinished`.
pub(super) async fn remote_delete(ctx: JobCtx) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let (file, original_file, locked, lock_token, wants, is_dir) = {
        let i = ctx.item.borrow();
        (
            i.file.clone(),
            i.original_file.clone(),
            i.locked,
            i.lock_token.clone(),
            i.wants_specific_actions,
            i.is_directory(),
        )
    };
    let mut headers = Vec::new();
    if locked == LockStatus::LockedItem {
        headers.push(("If", lock_if_header(&ctx, &file, &lock_token)));
    }
    ctx.shared.active_add(ctx.id, false);
    let reply = nc_dav::jobs::delete(
        &ctx.shared.account,
        &Target::Dav(ctx.shared.full_remote_path(&file)),
        &headers,
        wants == SynchronizationOptions::WantsPermanentDeletion,
        &opts(&ctx),
    )
    .await;
    ctx.shared.active_remove(ctx.id);
    let err = reply.error;
    let http_status = reply.http_status;
    {
        let mut i = ctx.item.borrow_mut();
        i.http_error_code = http_status;
        i.response_time_stamp = reply.response_timestamp();
        i.request_id = reply.request_id.as_bytes().to_vec();
    }
    if err != NetworkError::NoError && err != NetworkError::ContentNotFoundError {
        let status = classify_error(err, http_status, Some(&ctx.shared.another_sync_needed), &[]);
        return Some(Done::new(
            status,
            reply.error_string(),
            error_category_from_network_error(err),
        ));
    }
    // A 404 reply is also considered a success here: We want to make sure
    // a file is gone from the server. It not being there in the first place
    // is ok.
    if http_status != 204 && http_status != 404 {
        // Normally we expect "204 No Content"
        return Some(Done::new(
            Status::NormalError,
            format!(
                "Wrong HTTP code returned by server. Expected 204, but received \"{} {}\".",
                http_status, reply.reason_phrase
            ),
            ErrorCategory::GenericError,
        ));
    }
    if ctx
        .shared
        .journal
        .delete_file_record(&original_file, is_dir)
        .is_err()
    {
        log::warn!(target: "nextcloud.sync.propagator.remotedelete", "could not delete file from local DB {original_file}");
        return Some(Done::new(
            Status::NormalError,
            format!("Could not delete file record {original_file} from local DB"),
            ErrorCategory::GenericError,
        ));
    }
    ctx.shared.journal.commit("Remote Remove", true);
    Some(Done::ok())
}

/// `PropagateRemoteMkdir::start` .. `success`.
pub(super) async fn remote_mkdir(ctx: JobCtx) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let file = ctx.item.borrow().file.clone();
    ctx.shared.active_add(ctx.id, false);
    if ctx.delete_existing {
        let _ = nc_dav::jobs::delete(
            &ctx.shared.account,
            &Target::Dav(ctx.shared.full_remote_path(&file)),
            &[],
            false,
            &opts(&ctx),
        )
        .await;
    }
    // slotMkdir
    let (original_file, rename_target) = {
        let i = ctx.item.borrow();
        (i.original_file.clone(), i.rename_target.clone())
    };
    if !original_file.is_empty() && !rename_target.is_empty() && rename_target != original_file {
        let existing_file = ctx
            .shared
            .full_local_path(&ctx.shared.adjust_renamed_path(&original_file));
        let target_file = ctx.shared.full_local_path(&rename_target);
        if let Err(e) = filesystem::rename(&existing_file, &target_file) {
            ctx.shared.active_remove(ctx.id);
            return Some(Done::new(
                Status::NormalError,
                e,
                ErrorCategory::GenericError,
            ));
        }
        ctx.shared
            .emit(super::PropagatorEvent::TouchedFile(existing_file));
        ctx.shared
            .emit(super::PropagatorEvent::TouchedFile(target_file));
    }
    // slotStartMkcolJob
    if ctx.shared.abort_requested.get() {
        ctx.shared.active_remove(ctx.id);
        return None;
    }
    let dav_path = ctx.shared.full_remote_path(&file);
    let reply = nc_dav::jobs::mkcol(
        &ctx.shared.account,
        &Target::Dav(dav_path.clone()),
        &[],
        &opts(&ctx),
    )
    .await;
    // slotMkcolJobFinished
    ctx.shared.active_remove(ctx.id);
    let err = reply.error;
    {
        let mut i = ctx.item.borrow_mut();
        i.http_error_code = reply.http_status;
        i.response_time_stamp = reply.response_timestamp();
        i.request_id = reply.request_id.as_bytes().to_vec();
        i.file_id = reply.raw_header("OC-FileId").to_vec();
        i.error_string = reply.error_string();
    }
    // finalizeMkColJob
    let http_code = ctx.item.borrow().http_error_code;
    if http_code == 405 {
        // This happens when the directory already exists. Nothing to do.
        log::debug!(target: "nextcloud.sync.propagator.remotemkdir", "Folder {dav_path} already exists.");
    } else if err != NetworkError::NoError {
        let status = classify_error(err, http_code, Some(&ctx.shared.another_sync_needed), &[]);
        let msg = ctx.item.borrow().error_string.clone();
        return Some(Done::new(
            status,
            msg,
            error_category_from_network_error(err),
        ));
    } else if http_code != 201 {
        // Normally we expect "201 Created"
        return Some(Done::new(
            Status::NormalError,
            format!(
                "Wrong HTTP code returned by server. Expected 201, but received \"{} {}\".",
                http_code, reply.reason_phrase
            ),
            ErrorCategory::GenericError,
        ));
    }
    ctx.shared.active_add(ctx.id, false);
    let result = nc_dav::jobs::propfind(
        &ctx.shared.account,
        &dav_path,
        &[
            "http://owncloud.org/ns:share-types",
            "http://owncloud.org/ns:permissions",
            "http://nextcloud.org/ns:is-mount-root",
        ],
        &opts(&ctx),
    )
    .await;
    ctx.shared.active_remove(ctx.id);
    match result {
        Ok(result) => {
            let algorithm = if ctx.shared.account.server_has_mount_root_property() {
                MountedPermissionAlgorithm::UseMountRootProperty
            } else {
                MountedPermissionAlgorithm::WildGuessMountedSubProperty
            };
            let mut i = ctx.item.borrow_mut();
            i.remote_perm = RemotePermissions::from_server_string_with(
                result.get("permissions").map_or("", String::as_str),
                algorithm,
                &result,
            );
            i.shared_by_me = !result.get("share-types").is_none_or(|v| v.is_empty());
            i.is_shared = i.remote_perm.has_permission(Permission::IsShared) || i.shared_by_me;
            i.last_share_state_fetched_timestamp = current_msecs_since_epoch();
        }
        Err(reply) => {
            return Some(Done::new(
                Status::NormalError,
                "",
                error_category_from_network_error(reply.error),
            ));
        }
    }
    // success(): Never save the etag on first mkdir.
    // Only fully propagated directories should have the etag set.
    let mut item_copy: SyncFileItem = ctx.item.borrow().clone();
    item_copy.etag.clear();
    // save the file id already so we can detect rename or remove
    if let Err(e) = ctx.shared.update_metadata(&item_copy) {
        return Some(Done::new(
            Status::FatalError,
            format!("Error writing metadata to the database: {e}"),
            ErrorCategory::GenericError,
        ));
    }
    Some(Done::ok())
}

/// `PropagateRemoteMove::start` + `slotMoveJobFinished`.
pub(super) async fn remote_move(ctx: JobCtx) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let (file, rename_target, original_file) = {
        let i = ctx.item.borrow();
        (
            i.file.clone(),
            i.rename_target.clone(),
            i.original_file.clone(),
        )
    };
    let origin = ctx.shared.adjust_renamed_path(&file);
    if origin == rename_target {
        // The parent has been renamed already so there is nothing more to do.
        return finalize(&ctx);
    }
    let remote_source = ctx.shared.full_remote_path(&origin);
    let remote_destination = clean_path(&format!(
        "{}{}",
        ctx.shared.account.dav_url_path(),
        ctx.shared.full_remote_path(&rename_target)
    ));
    ctx.shared.active_add(ctx.id, false);
    let reply = nc_dav::jobs::move_(
        &ctx.shared.account,
        &Target::Dav(remote_source),
        &remote_destination,
        &[],
        &opts(&ctx),
    )
    .await;
    ctx.shared.active_remove(ctx.id);
    let err = reply.error;
    {
        let mut i = ctx.item.borrow_mut();
        i.http_error_code = reply.http_status;
        i.response_time_stamp = reply.response_timestamp();
        i.request_id = reply.request_id.as_bytes().to_vec();
    }
    let http_code = reply.http_status;
    if err != NetworkError::NoError {
        let status = classify_error(err, http_code, Some(&ctx.shared.another_sync_needed), &[]);
        let file_path = ctx.shared.full_local_path(&rename_target);
        let file_path_original = ctx.shared.full_local_path(&original_file);
        let _permissions = FilePermissionsRestore::new(
            &filesystem::parent_dir(&file_path_original),
            FolderPermissions::ReadWrite,
        );
        if filesystem::rename(&file_path, &file_path_original).is_err() {
            log::warn!(target: "nextcloud.sync.propagator.remotemove", "Could not MOVE file {file_path_original} to {file_path} with error: {} and failed to restore it !", reply.error_string());
        } else {
            log::warn!(target: "nextcloud.sync.propagator.remotemove", "Could not MOVE file {file_path_original} to {file_path} with error: {} and successfully restored it.", reply.error_string());
            let mut restored_item: SyncFileItem = ctx.item.borrow().clone();
            restored_item.rename_target = original_file.clone();
            if let Err(e) = ctx.shared.update_metadata(&restored_item) {
                return Some(Done::new(
                    Status::FatalError,
                    format!("Error updating metadata: {e}"),
                    ErrorCategory::GenericError,
                ));
            }
        }
        return Some(Done::new(
            status,
            reply.error_string(),
            ErrorCategory::GenericError,
        ));
    }
    if http_code != 201 {
        // Normally we expect "201 Created"
        return Some(Done::new(
            Status::NormalError,
            format!(
                "Wrong HTTP code returned by server. Expected 201, but received \"{} {}\".",
                http_code, reply.reason_phrase
            ),
            ErrorCategory::GenericError,
        ));
    }
    finalize(&ctx)
}

/// `QDir::cleanPath`: collapses `//`, resolves `.` and `..`, drops a
/// trailing `/`.
pub(crate) fn clean_path(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|s| *s != "..") {
                    out.pop();
                } else if !absolute {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// `PropagateRemoteMove::finalize`.
fn finalize(ctx: &JobCtx) -> Outcome {
    let (file, rename_target, original_file, is_dir) = {
        let i = ctx.item.borrow();
        (
            i.file.clone(),
            i.rename_target.clone(),
            i.original_file.clone(),
            i.is_directory(),
        )
    };
    // Retrieve old db data.
    let old_record = match ctx.shared.journal.get_file_record(original_file.as_bytes()) {
        Ok(r) => r.unwrap_or_default(),
        Err(_) => {
            log::warn!(target: "nextcloud.sync.propagator.remotemove", "Could not get file from local DB {original_file}");
            return Some(Done::new(
                Status::NormalError,
                format!("Could not get file {original_file} from local DB"),
                ErrorCategory::GenericError,
            ));
        }
    };
    let target_file = ctx.shared.full_local_path(&rename_target);
    if filesystem::file_exists(&target_file) {
        // Delete old db data.
        if ctx
            .shared
            .journal
            .delete_file_record(&original_file, false)
            .is_err()
        {
            log::warn!(target: "nextcloud.sync.propagator.remotemove", "could not delete file from local DB {original_file}");
            return Some(Done::new(
                Status::NormalError,
                format!("Could not delete file record {original_file} from local DB"),
                ErrorCategory::GenericError,
            ));
        }
    }
    let mut new_item: SyncFileItem = ctx.item.borrow().clone();
    new_item.lock_token.clear();
    new_item.locked = LockStatus::UnlockedItem;
    if old_record.is_valid() {
        new_item.checksum_header = old_record.checksum_header.clone();
        if new_item.size != old_record.file_size {
            log::warn!(target: "nextcloud.sync.propagator.remotemove", "File sizes differ on server vs sync journal: {} {}", new_item.size, old_record.file_size);
            // the server might have claimed a different size, we take the old one from the DB
            new_item.size = old_record.file_size;
        }
    }
    if let Err(e) = ctx.shared.update_metadata(&new_item)
        && filesystem::file_exists(&target_file)
    {
        return Some(Done::new(
            Status::FatalError,
            format!("Error updating metadata: {e}"),
            ErrorCategory::GenericError,
        ));
    }
    if is_dir {
        ctx.shared
            .renamed_directories
            .borrow_mut()
            .insert(file.clone(), rename_target.clone());
        if !super::local::adjust_selective_sync(&ctx.shared.journal, &file, &rename_target) {
            return Some(Done::new(
                Status::FatalError,
                "Error writing metadata to the database",
                ErrorCategory::GenericError,
            ));
        }
    }
    ctx.shared.journal.commit("Remote Rename", true);
    Some(Done::ok())
}

#[cfg(test)]
mod tests {
    use super::clean_path;

    #[test]
    fn derived_clean_path() {
        assert_eq!(clean_path("/a//b/./c/"), "/a/b/c");
        assert_eq!(clean_path("/a/b/../c"), "/a/c");
        assert_eq!(clean_path("/"), "/");
    }
}
