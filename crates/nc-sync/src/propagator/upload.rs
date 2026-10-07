// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-FileCopyrightText: 2016 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/propagateupload.{h,cpp}`
// (PropagateUploadFileCommon, UploadDevice, PUTFileJob, PollJob),
// `propagateuploadv1.cpp` (PropagateUploadFileV1) and
// `propagateuploadng.cpp` (PropagateUploadFileNG, chunked upload v2) of
// nextcloud/desktop v34.0.5, without the end-to-end encryption paths.

//! Uploads: a plain PUT (v1, optionally with the old `-chunking-` scheme)
//! or a chunked upload v2 (MKCOL an upload folder, PUT the chunks, MOVE
//! `.file` to the destination), resumable from the journal's upload info.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use http::Method;
use nc_dav::{Body, JobOptions, NetworkError, Reply, Target};
use nc_journal::checksums::{make_checksum_header, parse_checksum_header, upload_checksum_enabled};
use nc_journal::journal::{PollInfo, UploadInfo};

use super::{
    Done, JobCtx, Outcome, PropagatorEvent, classify_error, get_etag_from_reply,
    get_exception_from_reply,
};
use crate::filesystem;
use crate::item::{ErrorCategory, Instruction, LockOwnerType, LockStatus, Status};

const LOG: &str = "nextcloud.sync.propagator.upload";

/// `UploadFileInfo`: the file that is actually sent.
#[derive(Clone, Debug, Default)]
struct UploadFileInfo {
    file: String,
    path: String,
    size: i64,
}

fn done(status: Status, msg: impl Into<String>) -> Outcome {
    Some(Done::new(status, msg, ErrorCategory::NoError))
}

/// `QFileInfo(path).path()`: the directory part, `.` without one.
fn qfileinfo_path(file: &str) -> String {
    match file.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(i) => file[..i].to_owned(),
        None => ".".to_owned(),
    }
}

/// `UploadDevice`: `size` bytes of `path` from `start`, read while sending.
fn upload_device(path: &str, start: i64, size: i64) -> Result<Body, String> {
    let disk_size = filesystem::get_size(path);
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(start.max(0) as u64))
        .map_err(|e| e.to_string())?;
    let size = size.clamp(0, (disk_size - start).max(0));
    let mut remaining = size as u64;
    let stream = futures_util::stream::iter(std::iter::from_fn(move || {
        if remaining == 0 {
            return None;
        }
        let n = remaining.min(64 * 1024) as usize;
        let mut buf = vec![0u8; n];
        match f.read(&mut buf) {
            Ok(0) => None,
            Ok(read) => {
                buf.truncate(read);
                remaining -= read as u64;
                Some(Ok(Bytes::from(buf)))
            }
            Err(e) => {
                remaining = 0;
                Some(Err(e))
            }
        }
    }));
    Ok(Body::from_stream(stream, Some(size as u64)))
}

/// `fileIsStillChanging(item)`.
fn file_is_still_changing(modtime: i64, minimum_age: Duration) -> bool {
    let now_ms = jiff::Timestamp::now().as_millisecond();
    let ms_since_mod = now_ms - modtime * 1000;
    ms_since_mod < minimum_age.as_millis() as i64
        // if the mtime is too much in the future we *do* upload the file
        && ms_since_mod > -10000
}

/// `adjustLastJobTimeout(job, fileSize)`.
fn adjust_last_job_timeout(opts: &mut JobOptions, file_size: i64) {
    const THREE_MINUTES: f64 = 3.0 * 60.0 * 1000.0;
    const THIRTY_MINUTES: i64 = 30 * 60 * 1000;
    let lo = (THIRTY_MINUTES - 1).min((THREE_MINUTES * file_size as f64 / 1e9).round() as i64);
    let val = opts.timeout.as_millis() as i64;
    let ms = lo.max(val.min(THIRTY_MINUTES));
    opts.timeout = Duration::from_millis(ms as u64);
}

/// State of an upload (`PropagateUploadFileCommon` members).
struct Upload<'a> {
    ctx: &'a JobCtx,
    file_to_upload: UploadFileInfo,
    transmission_checksum_header: Vec<u8>,
    finished: bool,
}

impl Upload<'_> {
    fn quick(&self) -> bool {
        self.ctx.item.borrow().size < super::Shared::small_file_size()
    }

    fn opts(&self) -> JobOptions {
        JobOptions::with_cancel(self.ctx.shared.soft_abort.child_token())
    }

    /// `headers()`: the base headers of the PUT, or of the MOVE for chunking-ng.
    fn headers(&self) -> Vec<(&'static str, Vec<u8>)> {
        let i = self.ctx.item.borrow();
        let mut headers: Vec<(&'static str, Vec<u8>)> = vec![
            ("Content-Type", b"application/octet-stream".to_vec()),
            ("X-OC-Mtime", i.modtime.to_string().into_bytes()),
        ];
        if std::env::var("OWNCLOUD_LAZYOPS")
            .ok()
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
            != 0
        {
            headers.push(("OC-LazyOps", b"true".to_vec()));
        }
        if !i.etag.is_empty()
            && i.etag != b"empty_etag"
            && i.instruction != Instruction::New // On new files never send a If-Match
            && i.instruction != Instruction::TypeChange
            && !self.ctx.delete_existing
        {
            // We add quotes because the owncloud server always adds quotes around the etag
            let mut v = b"\"".to_vec();
            v.extend_from_slice(&i.etag);
            v.push(b'"');
            headers.push(("If-Match", v));
        }
        // Set up a conflict file header pointing to the original file
        let conflict_record = self.ctx.shared.journal.conflict_record(i.file.as_bytes());
        if conflict_record.is_valid() {
            headers.push(("OC-Conflict", b"1".to_vec()));
            if !conflict_record.initial_base_path.is_empty() {
                headers.push((
                    "OC-ConflictInitialBasePath",
                    conflict_record.initial_base_path.clone(),
                ));
            }
            if !conflict_record.base_file_id.is_empty() {
                headers.push((
                    "OC-ConflictBaseFileId",
                    conflict_record.base_file_id.clone(),
                ));
            }
            if conflict_record.base_modtime != -1 {
                headers.push((
                    "OC-ConflictBaseMtime",
                    conflict_record.base_modtime.to_string().into_bytes(),
                ));
            }
            if !conflict_record.base_etag.is_empty() {
                headers.push(("OC-ConflictBaseEtag", conflict_record.base_etag.clone()));
            }
        }
        headers
    }

    /// The `If` header for a token lock.
    fn token_lock_if_header(&self) -> Option<Vec<u8>> {
        let i = self.ctx.item.borrow();
        if i.lock_owner_type == LockOwnerType::TokenLock
            && i.locked == LockStatus::LockedItem
            && i.instruction != Instruction::New
            && i.instruction != Instruction::TypeChange
        {
            return Some(
                format!(
                    "<{}{}> (<opaquelocktoken:{}>)",
                    self.ctx.shared.account.dav_url_string(),
                    self.file_to_upload.file,
                    i.lock_token
                )
                .into_bytes(),
            );
        }
        None
    }

    /// `checkResettingErrors()`.
    fn check_resetting_errors(&self) {
        let (code, file) = {
            let i = self.ctx.item.borrow();
            (i.http_error_code, i.file.clone())
        };
        if code == 412
            || self
                .ctx
                .shared
                .account
                .capabilities()
                .http_error_codes_that_reset_failing_chunked_uploads()
                .contains(&(code as i32))
        {
            let mut upload_info = self.ctx.shared.journal.get_upload_info(&file);
            upload_info.error_count += 1;
            if upload_info.error_count > 3 {
                log::info!(target: LOG, "Reset transfer of {file} due to repeated error {code}");
                upload_info = UploadInfo::default();
            } else {
                log::info!(target: LOG, "Error count for maybe-reset error {code} on file {file} is {}", upload_info.error_count);
            }
            self.ctx.shared.journal.set_upload_info(&file, &upload_info);
            self.ctx.shared.journal.commit("Upload info", true);
        }
    }

    /// `commonErrorHandling(job)` + `abortWithError`.
    fn common_error_handling(&self, reply: &Reply) -> Outcome {
        let mut error_string = reply.error_string_parsing_body();
        log::warn!(target: LOG, "{}", String::from_utf8_lossy(&reply.body));
        let (code, file) = {
            let i = self.ctx.item.borrow();
            (i.http_error_code, i.file.clone())
        };
        if code == 412 || code == 423 {
            // Clear any stale lock token from the journal.
            if let Ok(Some(mut record)) = self.ctx.shared.journal.get_file_record(file.as_bytes())
                && record.is_valid()
                && !record.lockstate.lock_token.is_empty()
            {
                record.lockstate.lock_token.clear();
                record.lockstate.locked = false;
                if let Err(e) = self.ctx.shared.journal.set_file_record(&record) {
                    log::warn!(target: LOG, "Failed to clear stale lock token for {file} {e}");
                }
            }
            {
                let mut i = self.ctx.item.borrow_mut();
                i.lock_token.clear();
                i.locked = LockStatus::UnlockedItem;
            }
            self.ctx.shared.another_sync_needed.set(true);
            if code == 412 {
                self.ctx
                    .shared
                    .journal
                    .schedule_path_for_remote_discovery(file.as_bytes());
            }
        }
        // Ensure errors that should eventually reset the chunked upload are tracked.
        self.check_resetting_errors();
        let mut status = classify_error(
            reply.error,
            code,
            Some(&self.ctx.shared.another_sync_needed),
            &reply.body,
        );
        // Insufficient remote storage.
        if code == 507 {
            // Update the quota expectation
            let path = qfileinfo_path(&file);
            {
                let mut q = self.ctx.shared.folder_quota.borrow_mut();
                let v = q.entry(path).or_insert(i64::MAX);
                *v = (*v).min(self.file_to_upload.size - 1);
            }
            // Set up the error
            status = Status::DetailError;
            error_string = format!(
                "Upload of {} exceeds the quota for the folder",
                crate::utility::octets_to_string(self.file_to_upload.size)
            );
            self.ctx
                .shared
                .emit(PropagatorEvent::InsufficientRemoteStorage);
        } else if code == 400 {
            let exception = reply.error_string_parsing_body_exception();
            if exception.ends_with("\\InvalidPath") {
                error_string = "Unable to upload an item with invalid characters".to_owned();
                status = Status::FileNameInvalidOnServer;
            }
        }
        let (name, message) = get_exception_from_reply(reply);
        {
            let mut i = self.ctx.item.borrow_mut();
            i.error_exception_name = name;
            i.error_exception_message = message;
        }
        done(status, error_string)
    }

    /// `finalize()`.
    fn finalize(&self) -> Outcome {
        let file = self.ctx.item.borrow().file.clone();
        // Update the quota, if known
        if let Some(v) = self
            .ctx
            .shared
            .folder_quota
            .borrow_mut()
            .get_mut(&qfileinfo_path(&file))
        {
            *v -= self.file_to_upload.size;
        }
        // Update the database entry
        if let Err(e) = self.ctx.shared.update_metadata(&self.ctx.item.borrow()) {
            return done(Status::FatalError, format!("Error updating metadata: {e}"));
        }
        // Remove from the progress database:
        self.ctx
            .shared
            .journal
            .set_upload_info(&file, &UploadInfo::default());
        self.ctx.shared.journal.commit("upload file start", true);
        done(Status::Success, "")
    }

    /// `startPollJob(path)` + `PollJob` + `slotPollFinished`.
    async fn poll(&self, path: &str) -> Outcome {
        let (file, modtime, size) = {
            let i = self.ctx.item.borrow();
            (i.file.clone(), i.modtime, i.size)
        };
        self.ctx.shared.journal.set_poll_info(&PollInfo {
            file: file.clone(),
            url: path.to_owned(),
            modtime,
            file_size: size,
        });
        self.ctx.shared.journal.commit("add poll info", true);
        self.ctx.shared.active_add(self.ctx.id, self.quick());
        let result = poll_job(self.ctx, path).await;
        self.ctx.shared.active_remove(self.ctx.id);
        let (status, err) = {
            let i = self.ctx.item.borrow();
            (i.status, i.error_string.clone())
        };
        if status != Status::Success {
            let _ = result;
            return done(status, err);
        }
        self.finalize()
    }
}

/// `PollJob::start`/`finished` until the server reports the end of an
/// asynchronous PUT/MOVE. Sets the item status, etag and file id.
pub(crate) async fn poll_job(ctx: &JobCtx, path: &str) -> Option<()> {
    let account = &ctx.shared.account;
    loop {
        let full = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        let opts = JobOptions {
            cancel: Some(ctx.shared.soft_abort.child_token()),
            timeout: Duration::from_secs(120),
        };
        let reply = nc_dav::jobs::send(
            account,
            Method::GET,
            &Target::Absolute(full),
            http::HeaderMap::new(),
            Body::empty(),
            &opts,
        )
        .await;
        if !reply.is_ok() {
            let finished = {
                let mut i = ctx.item.borrow_mut();
                i.http_error_code = reply.http_status;
                i.request_id = reply.request_id.as_bytes().to_vec();
                i.status = classify_error(reply.error, i.http_error_code, None, &[]);
                i.error_string = reply.error_string();
                let (name, message) = get_exception_from_reply(&reply);
                i.error_exception_name = name;
                i.error_exception_message = message;
                if i.status == Status::FatalError || i.http_error_code >= 400 {
                    if i.status != Status::FatalError && i.http_error_code != 503 {
                        // no info._url removes it from the database
                        ctx.shared.journal.set_poll_info(&PollInfo {
                            file: i.file.clone(),
                            ..PollInfo::default()
                        });
                        ctx.shared.journal.commit("remove poll info", true);
                    }
                    true
                } else {
                    false
                }
            };
            if finished {
                return Some(());
            }
            tokio::time::sleep(Duration::from_secs(8)).await;
            continue;
        }
        let json: serde_json::Value = match serde_json::from_slice(trim(&reply.body)) {
            Ok(v) => v,
            Err(_) => {
                let mut i = ctx.item.borrow_mut();
                i.error_string = "Invalid JSON reply from the poll URL".to_owned();
                i.status = Status::NormalError;
                return Some(());
            }
        };
        let status = json
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_owned();
        if status == "init" || status == "started" {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let mut i = ctx.item.borrow_mut();
        i.response_time_stamp = reply.response_timestamp();
        i.http_error_code = json.get("errorCode").and_then(|v| v.as_i64()).unwrap_or(0) as u16;
        if status == "finished" {
            i.status = Status::Success;
            i.file_id = json
                .get("fileId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            let dest = i.destination().to_owned();
            if let Ok(Some(old)) = ctx.shared.journal.get_file_record(dest.as_bytes())
                && old.is_valid()
                && old.etag != i.etag
            {
                i.update_lock_state_from_db_record(&old);
            }
            i.etag = nc_dav::xml::parse_etag(
                json.get("ETag")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            );
        } else {
            // error
            i.status = classify_error(
                NetworkError::UnknownContentError,
                i.http_error_code,
                None,
                &[],
            );
            i.error_string = json
                .get("errorMessage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
        }
        ctx.shared.journal.set_poll_info(&PollInfo {
            file: i.file.clone(),
            ..PollInfo::default()
        });
        ctx.shared.journal.commit("remove poll info", true);
        return Some(());
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(start, |e| e + 1);
    &b[start..end]
}

/// The whole upload (`PropagateUploadFileCommon::start` and the v1/ng state
/// machines).
pub(super) async fn upload(ctx: JobCtx, ng: bool) -> Outcome {
    let mut up = Upload {
        ctx: &ctx,
        file_to_upload: UploadFileInfo::default(),
        transmission_checksum_header: Vec::new(),
        finished: false,
    };
    let outcome = upload_common(&mut up, ng).await;
    // Make sure no stale entries of this job stay in the active list.
    while ctx
        .shared
        .active_job_list
        .borrow()
        .iter()
        .any(|(j, _)| *j == ctx.id)
    {
        ctx.shared.active_remove(ctx.id);
    }
    outcome
}

async fn upload_common(up: &mut Upload<'_>, ng: bool) -> Outcome {
    let ctx = up.ctx;
    let (original_file, rename_target, file) = {
        let i = ctx.item.borrow();
        (
            i.original_file.clone(),
            i.rename_target.clone(),
            i.file.clone(),
        )
    };
    if !original_file.is_empty() && !rename_target.is_empty() && rename_target != original_file {
        let existing_file = ctx
            .shared
            .full_local_path(&ctx.shared.adjust_renamed_path(&original_file));
        let target_file = ctx.shared.full_local_path(&rename_target);
        if let Err(e) = filesystem::rename(&existing_file, &target_file) {
            return done(Status::NormalError, e);
        }
        ctx.shared.emit(PropagatorEvent::TouchedFile(existing_file));
        ctx.shared.emit(PropagatorEvent::TouchedFile(target_file));
    }
    let parent_path = match file.rfind('/') {
        Some(i) => file[..i].to_owned(),
        None => String::new(),
    };
    if ctx
        .shared
        .journal
        .get_file_record(parent_path.as_bytes())
        .is_err()
    {
        return done(Status::NormalError, "");
    }
    // setupUnencryptedFile
    up.file_to_upload = UploadFileInfo {
        file: file.clone(),
        size: ctx.item.borrow().size,
        path: ctx.shared.full_local_path(&file),
    };
    // startUploadFile
    if ctx.shared.abort_requested.get() {
        return None;
    }
    // Check if we believe that the upload will fail due to remote quota limits
    let quota_guess = ctx
        .shared
        .folder_quota
        .borrow()
        .get(&qfileinfo_path(&up.file_to_upload.file))
        .copied()
        .unwrap_or(i64::MAX);
    if up.file_to_upload.size > quota_guess {
        // Necessary for blacklisting logic
        ctx.item.borrow_mut().http_error_code = 507;
        ctx.shared.emit(PropagatorEvent::InsufficientRemoteStorage);
        return done(
            Status::DetailError,
            format!(
                "Upload of {} exceeds the quota for the folder",
                crate::utility::octets_to_string(up.file_to_upload.size)
            ),
        );
    }
    ctx.shared.active_add(ctx.id, up.quick());
    if ctx.delete_existing {
        let _ = nc_dav::jobs::delete(
            &ctx.shared.account,
            &Target::Dav(ctx.shared.full_remote_path(&up.file_to_upload.file)),
            &[],
            false,
            &up.opts(),
        )
        .await;
    }
    // slotComputeContentChecksum
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let file_path = ctx.shared.full_local_path(&file);
    // remember the modtime before checksumming to be able to detect a file
    // change during the checksum calculation
    let mut modtime = filesystem::get_mod_time(&file_path);
    if modtime <= 0 {
        let now = crate::utility::current_secs_since_epoch();
        if filesystem::set_mod_time(&file_path, now) {
            modtime = now;
        } else {
            log::warn!(target: LOG, "Could not update modification time for {file}");
            return done(
                Status::NormalError,
                format!("File {file} has invalid modification time. Do not upload to the server."),
            );
        }
    }
    ctx.item.borrow_mut().modtime = modtime;
    let caps = ctx.shared.account.capabilities();
    let checksum_type = caps.preferred_upload_checksum_type();
    // Maybe the discovery already computed the checksum?
    let existing = parse_checksum_header(&ctx.item.borrow().checksum_header).unwrap_or_default();
    let (content_type, content_checksum) = if existing.0 == checksum_type {
        (checksum_type.clone(), existing.1)
    } else {
        // Compute the content checksum.
        let mut c = nc_journal::checksums::ComputeChecksum::new();
        c.set_checksum_type(&checksum_type);
        c.start(std::path::Path::new(&up.file_to_upload.path))
    };
    // slotComputeTransmissionChecksum
    ctx.item.borrow_mut().checksum_header = make_checksum_header(&content_type, &content_checksum);
    // Reuse the content checksum as the transmission checksum if possible
    let supported = caps.supported_checksum_types();
    let (transmission_type, transmission_checksum) = if supported.contains(&content_type) {
        (content_type, content_checksum)
    } else {
        // Compute the transmission checksum.
        let mut c = nc_journal::checksums::ComputeChecksum::new();
        if upload_checksum_enabled() {
            c.set_checksum_type(&caps.upload_checksum_type());
        } else {
            c.set_checksum_type(b"");
        }
        c.start(std::path::Path::new(&up.file_to_upload.path))
    };
    // slotStartUpload
    // Remove ourselves from the list of active job, before any possible call to done()
    ctx.shared.active_remove(ctx.id);
    up.transmission_checksum_header =
        make_checksum_header(&transmission_type, &transmission_checksum);
    // If no checksum header was not set, reuse the transmission checksum as the content checksum.
    if ctx.item.borrow().checksum_header.is_empty() {
        ctx.item.borrow_mut().checksum_header = up.transmission_checksum_header.clone();
    }
    let full_file_path = up.file_to_upload.path.clone();
    let original_file_path = ctx.shared.full_local_path(&file);
    if !filesystem::file_exists(&full_file_path) {
        return done(
            Status::SoftError,
            format!("File Removed (start upload) {full_file_path}"),
        );
    }
    let prev_modtime = ctx.item.borrow().modtime;
    if prev_modtime <= 0 {
        return done(
            Status::NormalError,
            format!("File {file} has invalid modification time. Do not upload to the server."),
        );
    }
    // a potential checksum calculation could have taken some time during which the file could
    // have been changed again, so better check again here.
    let new_modtime = filesystem::get_mod_time(&original_file_path);
    ctx.item.borrow_mut().modtime = new_modtime;
    if new_modtime <= 0 {
        return done(
            Status::NormalError,
            format!("File {file} has invalid modification time. Do not upload to the server."),
        );
    }
    if prev_modtime != new_modtime {
        ctx.shared.another_sync_needed.set(true);
        return done(
            Status::SoftError,
            "Local file changed during syncing. It will be resumed.",
        );
    }
    // for new uploads also ensure the file sizes stays the same
    let prev_file_to_upload_size = up.file_to_upload.size;
    let prev_item_size = ctx.item.borrow().size;
    up.file_to_upload.size = filesystem::get_size(&full_file_path);
    let new_item_size = filesystem::get_size(&original_file_path);
    ctx.item.borrow_mut().size = new_item_size;
    let instruction = ctx.item.borrow().instruction;
    let file_sizes_changed_for_new_item = instruction == Instruction::New
        // file conflict items created during propagation may not have a file size, ignore those
        && !(prev_item_size == 0 && prev_file_to_upload_size == 0)
        && !(prev_file_to_upload_size == up.file_to_upload.size && prev_item_size == new_item_size);
    if file_sizes_changed_for_new_item {
        log::warn!(target: LOG, "File sizes changed between discovery and propagation phase fileToUpload.path={} fileToUpload.size={} prevFileToUploadSize={prev_file_to_upload_size} item.file={file} item.size={new_item_size} prevItemSize={prev_item_size}", up.file_to_upload.path, up.file_to_upload.size);
    }
    // But skip the file if the mtime is too close to 'now'!
    if file_is_still_changing(new_modtime, ctx.shared.options.minimum_file_age_for_upload)
        || file_sizes_changed_for_new_item
    {
        ctx.shared.another_sync_needed.set(true);
        return done(Status::SoftError, "Local file changed during sync.");
    }
    if ng {
        upload_ng(up).await
    } else {
        upload_v1(up).await
    }
}

/// `PropagateUploadFileV1::doStartUpload` .. `slotPutFinished`.
async fn upload_v1(up: &mut Upload<'_>) -> Outcome {
    let ctx = up.ctx;
    let chunk_size = ctx.shared.options.initial_chunk_size;
    let file_size = up.file_to_upload.size;
    let chunk_count = (file_size as f64 / chunk_size as f64).ceil() as i64;
    let mut start_chunk: i64 = 0;
    let (file, modtime, item_size, checksum_header) = {
        let i = ctx.item.borrow();
        (i.file.clone(), i.modtime, i.size, i.checksum_header.clone())
    };
    let mut transfer_id: u32 =
        fastrand::u32(0..0x7fff_ffff) ^ (modtime as u32) ^ ((file_size as u32) << 16);
    let progress_info = ctx.shared.journal.get_upload_info(&file);
    if progress_info.valid
        && progress_info.is_chunked()
        && progress_info.modtime == modtime
        && progress_info.size == item_size
        && (progress_info.content_checksum == checksum_header
            || progress_info.content_checksum.is_empty()
            || checksum_header.is_empty())
    {
        start_chunk = i64::from(progress_info.chunk_upload_v1);
        transfer_id = progress_info.transferid;
    } else if chunk_count <= 1 && !checksum_header.is_empty() {
        // If there is only one chunk, write the checksum in the database, so if the PUT is sent
        // to the server, but the connection drops before we get the etag, we can check the checksum
        // in reconcile (issue #5106)
        ctx.shared.journal.set_upload_info(
            &file,
            &UploadInfo {
                valid: true,
                chunk_upload_v1: 0,
                transferid: 0, // We set a null transfer id because it is not chunked.
                modtime,
                error_count: 0,
                content_checksum: checksum_header.clone(),
                size: item_size,
            },
        );
        ctx.shared.journal.commit("Upload info", true);
    }
    let mut current_chunk: i64 = 0;
    ctx.shared.report_progress(&ctx.item.borrow().clone(), 0);
    let mut inflight_chunks: Vec<i64> = Vec::new();
    // In-flight PUTs: (chunk number as `_chunk`, reply).
    let mut jobs: FuturesUnordered<PutFuture<'_>> = FuturesUnordered::new();

    let parallel_chunk_upload_allowed = {
        if ctx
            .shared
            .account
            .capabilities()
            .chunking_parallel_upload_disabled()
        {
            // Server may also disable parallel chunked upload for any higher version
            false
        } else if let Ok(env) = std::env::var("OWNCLOUD_PARALLEL_CHUNK")
            && !env.is_empty()
        {
            env != "false" && env != "0"
        } else {
            // Disable parallel chunk upload severs older than 8.0.3
            ctx.shared.account.server_version_int() >= nc_dav::account::make_server_version(8, 0, 3)
        }
    };

    // startNextChunk; returns Some(outcome) when the job is done.
    macro_rules! start_next_chunk {
        () => {{
            let mut res: Option<Outcome> = None;
            loop {
                if ctx.shared.abort_requested.get() {
                    res = Some(None);
                    break;
                }
                if !jobs.is_empty() && current_chunk + start_chunk >= chunk_count - 1 {
                    // Don't do parallel upload of chunk if this might be the last chunk because
                    // the server cannot handle that. We will proceed with the last chunk when
                    // the _jobs are finished.
                    break;
                }
                let mut headers = up.headers();
                headers.push(("OC-Total-Length", file_size.to_string().into_bytes()));
                headers.push(("OC-Chunk-Size", chunk_size.to_string().into_bytes()));
                let mut path = up.file_to_upload.file.clone();
                if let Some(h) = up.token_lock_if_header() {
                    headers.push(("If", h));
                }
                let mut chunk_start = 0i64;
                let mut current_chunk_size = file_size;
                let mut is_final_chunk = false;
                if chunk_count > 1 {
                    let sending_chunk = (current_chunk + start_chunk) % chunk_count;
                    // XOR with chunk size to make sure everything goes well if chunk size changes between runs
                    let transid = transfer_id ^ (chunk_size as u32);
                    path.push_str(&format!("-chunking-{transid}-{chunk_count}-{sending_chunk}"));
                    headers.push(("OC-Chunked", b"1".to_vec()));
                    chunk_start = chunk_size * sending_chunk;
                    current_chunk_size = chunk_size;
                    if sending_chunk == chunk_count - 1 {
                        // last chunk
                        current_chunk_size = file_size % chunk_size;
                        if current_chunk_size == 0 {
                            // if the last chunk pretends to be 0, its actually the full chunk size.
                            current_chunk_size = chunk_size;
                        }
                        is_final_chunk = true;
                    }
                } else {
                    // if there's only one chunk, it's the final one
                    is_final_chunk = true;
                }
                if is_final_chunk && !up.transmission_checksum_header.is_empty() {
                    headers.push(("OC-Checksum", up.transmission_checksum_header.clone()));
                }
                let device = match upload_device(&up.file_to_upload.path, chunk_start, current_chunk_size) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!(target: "nextcloud.sync.propagator.upload.v1", "Could not prepare upload device: {e}");
                        // Soft error because this is likely caused by the user modifying his files while syncing
                        res = Some(done(Status::SoftError, e));
                        break;
                    }
                };
                let mut opts = if is_final_chunk {
                    JobOptions::with_cancel(ctx.shared.hard_abort.child_token())
                } else {
                    up.opts()
                };
                if is_final_chunk {
                    adjust_last_job_timeout(&mut opts, file_size);
                }
                let target = Target::Dav(ctx.shared.full_remote_path(&path));
                let account = ctx.shared.account.clone();
                let chunk_no = current_chunk;
                jobs.push(Box::pin(async move {
                    let reply = nc_dav::jobs::put(&account, &target, &headers, device, &opts).await;
                    (chunk_no, reply)
                }));
                inflight_chunks.push(chunk_no);
                ctx.shared.active_add(ctx.id, up.quick());
                current_chunk += 1;
                let mut parallel_chunk_upload = parallel_chunk_upload_allowed;
                if current_chunk + start_chunk >= chunk_count - 1 {
                    // Don't do parallel upload of chunk if this might be the last chunk
                    parallel_chunk_upload = false;
                }
                let continue_parallel = parallel_chunk_upload
                    && ctx.shared.active_job_list.borrow().len() < ctx.shared.maximum_active_transfer_job()
                    && current_chunk < chunk_count;
                if !parallel_chunk_upload || chunk_count - current_chunk <= 0 {
                    ctx.shared.schedule_next_job();
                }
                if !continue_parallel {
                    break;
                }
            }
            res
        }};
    }

    if let Some(outcome) = start_next_chunk!() {
        return outcome;
    }
    loop {
        let Some((job_chunk, reply)) = jobs.next().await else {
            // Nothing in flight: upstream would wait forever; report the abort.
            return None;
        };
        if reply.error == NetworkError::NoError {
            // slotUploadProgress(sent, total): the fake server (like a real
            // reply) reports the whole chunk as sent right before finishing.
            let sending_chunk = (job_chunk + start_chunk) % chunk_count.max(1);
            let sent_by_job = if chunk_count <= 1 {
                file_size
            } else if sending_chunk == chunk_count - 1 && file_size % chunk_size != 0 {
                file_size % chunk_size
            } else {
                chunk_size
            };
            let mut progress_chunk = current_chunk + start_chunk - 1;
            if progress_chunk >= chunk_count {
                progress_chunk = current_chunk - 1;
            }
            // amount is the number of bytes already sent by all the other chunks that were sent
            // not including this one.
            let mut amount = progress_chunk * chunk_size;
            let other_jobs = jobs.len() as i64;
            if other_jobs > 0 {
                // The other jobs still in flight have not reported any byte yet.
                amount -= other_jobs * chunk_size;
            }
            amount += sent_by_job;
            ctx.shared
                .report_progress(&ctx.item.borrow().clone(), amount);
        }
        // slotPutFinished
        if let Some(pos) = inflight_chunks.iter().position(|c| *c == job_chunk) {
            inflight_chunks.remove(pos);
        }
        ctx.shared.active_remove(ctx.id);
        if up.finished {
            // We have sent the finished signal already. We don't need to handle any remaining jobs
            continue;
        }
        {
            let mut i = ctx.item.borrow_mut();
            i.http_error_code = reply.http_status;
            i.response_time_stamp = reply.response_timestamp();
            i.request_id = reply.request_id.as_bytes().to_vec();
        }
        if reply.error != NetworkError::NoError {
            return up.common_error_handling(&reply);
        }
        // The server needs some time to process the request and provide us with a poll URL
        if reply.http_status == 202 {
            let path = reply.raw_header_str("OC-JobStatus-Location");
            if path.is_empty() {
                return done(Status::NormalError, "Poll URL missing");
            }
            up.finished = true;
            drop(jobs);
            return up.poll(&path).await;
        }
        // Check the file again post upload.
        let etag = get_etag_from_reply(&reply);
        up.finished = !etag.is_empty();
        // Check if the file still exists
        let full_file_path = ctx.shared.full_local_path(&file);
        if !filesystem::file_exists(&full_file_path) {
            if !up.finished {
                return done(Status::SoftError, "The local file was removed during sync.");
            } else {
                ctx.shared.another_sync_needed.set(true);
            }
        }
        // Check whether the file changed since discovery.
        let (size, mtime) = {
            let i = ctx.item.borrow();
            (i.size, i.modtime)
        };
        if !filesystem::verify_file_unchanged(&full_file_path, size, mtime) {
            ctx.shared.another_sync_needed.set(true);
            if !up.finished {
                return done(Status::SoftError, "Local file changed during sync.");
            }
        }
        if !up.finished {
            // Proceed to next chunk.
            if current_chunk >= chunk_count {
                if !jobs.is_empty() {
                    // just wait for the other job to finish.
                    continue;
                }
                return done(
                    Status::NormalError,
                    "The server did not acknowledge the last chunk. (No e-tag was present)",
                );
            }
            // Deletes an existing blacklist entry on successful chunk upload
            if ctx.item.borrow().has_blacklist_entry {
                ctx.shared.journal.wipe_error_blacklist_entry(&file);
                ctx.item.borrow_mut().has_blacklist_entry = false;
            }
            // Take the minimum finished one
            let mut done_chunk = job_chunk;
            for c in &inflight_chunks {
                done_chunk = done_chunk.min(c - 1);
            }
            let next = (done_chunk + start_chunk + 1).rem_euclid(chunk_count.max(1));
            ctx.shared.journal.set_upload_info(
                &file,
                &UploadInfo {
                    valid: true,
                    chunk_upload_v1: next as i32,
                    transferid: transfer_id,
                    modtime: mtime,
                    error_count: 0, // successful chunk upload resets
                    content_checksum: ctx.item.borrow().checksum_header.clone(),
                    size,
                },
            );
            ctx.shared.journal.commit("Upload info", true);
            if let Some(outcome) = start_next_chunk!() {
                return outcome;
            }
            continue;
        }
        // the following code only happens after all chunks were uploaded.
        // the file id should only be empty for new files up- or downloaded
        let fid = reply.raw_header("OC-FileID").to_vec();
        {
            let mut i = ctx.item.borrow_mut();
            if !fid.is_empty() {
                if !i.file_id.is_empty() && i.file_id != fid {
                    log::warn!(target: "nextcloud.sync.propagator.upload.v1", "File ID changed! {:?} {:?}", String::from_utf8_lossy(&i.file_id), String::from_utf8_lossy(&fid));
                }
                i.file_id = fid;
            }
            let dest = i.destination().to_owned();
            if let Ok(Some(old)) = ctx.shared.journal.get_file_record(dest.as_bytes())
                && old.is_valid()
                && old.etag != i.etag
            {
                i.update_lock_state_from_db_record(&old);
            }
            i.etag = etag;
        }
        if reply.raw_header("X-OC-MTime") != b"accepted" {
            // X-OC-MTime is supported since owncloud 5.0.   But not when chunking.
            log::warn!(target: "nextcloud.sync.propagator.upload.v1", "Server does not support X-OC-MTime {}", reply.raw_header_str("X-OC-MTime"));
        }
        drop(jobs);
        return up.finalize();
    }
}

/// An in-flight chunk PUT: (chunk number, reply).
type PutFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = (i64, Reply)> + 'a>>;

/// Information about a chunk on the server (`ServerChunkInfo`).
struct ServerChunkInfo {
    size: i64,
    original_name: String,
}

/// `PropagateUploadFileNG::doStartUpload` .. `slotMoveJobFinished`.
async fn upload_ng(up: &mut Upload<'_>) -> Outcome {
    let ctx = up.ctx;
    let account = ctx.shared.account.clone();
    let (file, modtime, item_size) = {
        let i = ctx.item.borrow();
        (i.file.clone(), i.modtime, i.size)
    };
    let uploads_root = format!("remote.php/dav/uploads/{}", account.dav_user());
    let chunk_upload_folder = |transfer_id: u32| format!("{uploads_root}/{transfer_id}");
    // `chunkUploadFolderUrl()` as a decoded absolute path.
    let folder_path =
        |transfer_id: u32| account.account_path_for(&chunk_upload_folder(transfer_id));
    // `destinationHeader()`
    let destination_header = || {
        let dav_url = nc_journal::utility::trailing_slash_path(&account.dav_url_string());
        let remote_path = nc_dav::account::to_percent_encoding_keep_slash(
            nc_journal::utility::no_leading_slash_path(
                &ctx.shared.full_remote_path(&up.file_to_upload.file),
            ),
        );
        format!("{dav_url}{remote_path}").into_bytes()
    };

    ctx.shared.active_add(ctx.id, up.quick());
    let progress_info = ctx.shared.journal.get_upload_info(&file);
    let mut transfer_id: u32 = 0;
    let mut sent: i64 = 0;
    let mut current_chunk: i64 = 1;
    let mut need_new_upload = true;
    if progress_info.valid
        && progress_info.is_chunked()
        && progress_info.modtime == modtime
        && progress_info.size == item_size
    {
        transfer_id = progress_info.transferid;
        let result = nc_dav::jobs::ls_col(
            &account,
            &Target::Absolute(folder_path(transfer_id)),
            &["resourcetype", "getcontentlength"],
            &up.opts(),
        )
        .await;
        match result {
            Ok((listing, _)) => {
                // slotPropfindIterate
                let mut server_chunks: BTreeMap<i64, ServerChunkInfo> = BTreeMap::new();
                let own_path = folder_path(transfer_id);
                for entry in &listing.entries {
                    if entry.href == own_path.trim_end_matches('/') {
                        continue; // skip the info about the path itself
                    }
                    let chunk_name =
                        entry.href[entry.href.rfind('/').map_or(0, |i| i + 1)..].to_owned();
                    if let Ok(chunk_id) = chunk_name.parse::<i64>() {
                        let size = crate::utility::qstring_to_long_long(
                            entry
                                .properties
                                .get("getcontentlength")
                                .map_or("", String::as_str),
                        );
                        server_chunks.insert(
                            chunk_id,
                            ServerChunkInfo {
                                size,
                                original_name: chunk_name,
                            },
                        );
                    }
                }
                // slotPropfindFinished
                ctx.shared.active_remove(ctx.id);
                // Chunked upload v2: numbers range from 1 to 10000
                current_chunk = 1;
                sent = 0;
                while let Some(c) = server_chunks.remove(&current_chunk) {
                    sent += c.size;
                    current_chunk += 1;
                }
                if sent > up.file_to_upload.size {
                    // Normally this can't happen because the size is xor'ed with the transfer id
                    log::error!(target: "nextcloud.sync.propagator.upload.ng", "Inconsistency while resuming {file}: the size on the server ({sent}) is bigger than the size of the file ({})", up.file_to_upload.size);
                    // Wipe the old chunking data. Fire and forget.
                    let _ = nc_dav::jobs::delete(
                        &account,
                        &Target::Absolute(folder_path(transfer_id)),
                        &[],
                        false,
                        &up.opts(),
                    )
                    .await;
                    ctx.shared.active_add(ctx.id, up.quick());
                    need_new_upload = true;
                } else if !server_chunks.is_empty() {
                    ctx.shared.active_add(ctx.id, up.quick());
                    let mut remove_job_error = false;
                    // Make sure that if there is a "hole" and then a few more chunks, on the server
                    // we should remove the later chunks.
                    let mut deletes = FuturesUnordered::new();
                    for chunk in server_chunks.values() {
                        let target = Target::Absolute(nc_dav::account::concat_url_path(
                            &folder_path(transfer_id),
                            &chunk.original_name,
                        ));
                        let account = account.clone();
                        let opts = up.opts();
                        deletes.push(async move {
                            nc_dav::jobs::delete(&account, &target, &[], false, &opts).await
                        });
                    }
                    while let Some(reply) = deletes.next().await {
                        // slotDeleteJobFinished
                        let err = reply.error;
                        if err != NetworkError::NoError && err != NetworkError::ContentNotFoundError
                        {
                            let status = classify_error(err, reply.http_status, None, &[]);
                            if status == Status::FatalError {
                                ctx.item.borrow_mut().request_id =
                                    reply.request_id.as_bytes().to_vec();
                                return done(status, reply.error_string());
                            }
                            log::warn!(target: "nextcloud.sync.propagator.upload.ng", "DeleteJob errored out {} {}", reply.error_string(), reply.url);
                            remove_job_error = true;
                        }
                    }
                    ctx.shared.active_remove(ctx.id);
                    // There was an error removing some files, just start over
                    need_new_upload = remove_job_error;
                } else {
                    need_new_upload = false;
                }
            }
            Err(reply) => {
                // slotPropfindFinishedWithError
                let status = classify_error(
                    reply.error,
                    reply.http_status,
                    Some(&ctx.shared.another_sync_needed),
                    &[],
                );
                if status == Status::FatalError && reply.error != NetworkError::NoError {
                    ctx.item.borrow_mut().request_id = reply.request_id.as_bytes().to_vec();
                    ctx.shared.active_remove(ctx.id);
                    return done(status, reply.error_string_parsing_body());
                }
                need_new_upload = true;
            }
        }
    } else if progress_info.valid && progress_info.is_chunked() {
        // The upload info is stale. remove the stale chunks on the server
        transfer_id = progress_info.transferid;
        // Fire and forget. Any error will be ignored.
        let _ = nc_dav::jobs::delete(
            &account,
            &Target::Absolute(folder_path(transfer_id)),
            &[],
            false,
            &up.opts(),
        )
        .await;
        // startNewUpload will reset the _transferId and the UploadInfo in the db.
    }

    if need_new_upload {
        // startNewUpload
        transfer_id = fastrand::u32(0..0x7fff_ffff)
            ^ (modtime as u32)
            ^ ((up.file_to_upload.size as u32) << 16)
            ^ fastrand::u32(..);
        sent = 0;
        current_chunk = 1; // Chunked upload v2: numbers range from 1 to 10000
        ctx.shared.report_progress(&ctx.item.borrow().clone(), 0);
        ctx.shared.journal.set_upload_info(
            &file,
            &UploadInfo {
                valid: true,
                transferid: transfer_id,
                modtime,
                content_checksum: ctx.item.borrow().checksum_header.clone(),
                size: item_size,
                chunk_upload_v1: 0,
                error_count: 0,
            },
        );
        ctx.shared.journal.commit("Upload info", true);
        let headers: Vec<(&str, Vec<u8>)> = vec![
            (
                "OC-Total-Length",
                up.file_to_upload.size.to_string().into_bytes(),
            ),
            ("Destination", destination_header()),
        ];
        let reply = nc_dav::jobs::mkcol(
            &account,
            &Target::Absolute(folder_path(transfer_id)),
            &headers,
            &up.opts(),
        )
        .await;
        // slotMkColFinished
        ctx.shared.active_remove(ctx.id);
        ctx.item.borrow_mut().http_error_code = reply.http_status;
        if reply.error != NetworkError::NoError || reply.http_status != 201 {
            ctx.item.borrow_mut().request_id = reply.request_id.as_bytes().to_vec();
            let status = if reply.error == NetworkError::NoError {
                // classifyError asserts an error; a non-201 success maps like a content error.
                classify_error(
                    NetworkError::UnknownContentError,
                    reply.http_status,
                    Some(&ctx.shared.another_sync_needed),
                    &[],
                )
            } else {
                classify_error(
                    reply.error,
                    reply.http_status,
                    Some(&ctx.shared.another_sync_needed),
                    &[],
                )
            };
            return done(status, reply.error_string_parsing_body());
        }
    }

    // startNextChunk / slotPutFinished loop
    loop {
        if ctx.shared.abort_requested.get() {
            return None;
        }
        let file_size = up.file_to_upload.size;
        // prevent situation that chunk size is bigger then required one to send
        let current_chunk_size = ctx.shared.chunk_size.get().min(file_size - sent);
        if current_chunk_size == 0 {
            break; // finishUpload
        }
        let device = match upload_device(&up.file_to_upload.path, sent, current_chunk_size) {
            Ok(d) => d,
            Err(e) => {
                log::warn!(target: "nextcloud.sync.propagator.upload.ng", "Could not prepare upload device: {e}");
                // Soft error because this is likely caused by the user modifying his files while syncing
                return done(Status::SoftError, e);
            }
        };
        let headers: Vec<(&str, Vec<u8>)> = vec![
            ("OC-Chunk-Offset", sent.to_string().into_bytes()),
            ("Destination", destination_header()),
            ("OC-Total-Length", file_size.to_string().into_bytes()),
        ];
        sent += current_chunk_size;
        // We need to do add leading 0 because the server orders the chunk alphabetically
        let url = nc_dav::account::concat_url_path(
            &folder_path(transfer_id),
            &format!("{current_chunk:05}"),
        );
        ctx.shared.active_add(ctx.id, up.quick());
        current_chunk += 1;
        let started = Instant::now();
        let reply = nc_dav::jobs::put(
            &account,
            &Target::Absolute(url),
            &headers,
            device,
            &up.opts(),
        )
        .await;
        if reply.error == NetworkError::NoError {
            // slotUploadProgress(sent, total): the fake server (like a real
            // reply) reports the whole chunk as sent right before finishing.
            ctx.shared.report_progress(&ctx.item.borrow().clone(), sent);
            if ctx.shared.abort_requested.get() {
                // The receiver aborted the sync: the reply is aborted
                // before it finishes.
                ctx.shared.active_remove(ctx.id);
                return None;
            }
        }
        // slotPutFinished
        ctx.shared.active_remove(ctx.id);
        if reply.error != NetworkError::NoError {
            {
                let mut i = ctx.item.borrow_mut();
                i.http_error_code = reply.http_status;
                i.request_id = reply.request_id.as_bytes().to_vec();
            }
            return up.common_error_handling(&reply);
        }
        // Adjust the chunk size for the time taken.
        let target_duration = ctx.shared.options.target_chunk_upload_duration;
        if !target_duration.is_zero() {
            let upload_time = started.elapsed().as_millis() as i64 + 1; // add one to avoid div-by-zero
            let predicted_good_size = (current_chunk_size as i128
                * target_duration.as_millis() as i128
                / upload_time as i128) as i64;
            // We use an exponential moving average here as a cheap way of smoothing
            let target_size = ctx.shared.chunk_size.get() / 2 + predicted_good_size / 2;
            let min = ctx.shared.options.min_chunk_size();
            let max = ctx.shared.options.max_chunk_size();
            ctx.shared
                .chunk_size
                .set(target_size.max(min).min(max.max(min)));
            log::info!(target: "nextcloud.sync.propagator.upload.ng", "Chunked upload of {current_chunk_size} bytes took {upload_time}ms, desired is {}ms, expected good chunk size is {predicted_good_size} bytes and nudged next chunk size to {} bytes", target_duration.as_millis(), ctx.shared.chunk_size.get());
        }
        up.finished = sent == ctx.item.borrow().size;
        // Check if the file still exists
        let full_file_path = ctx.shared.full_local_path(&file);
        if !filesystem::file_exists(&full_file_path) {
            if !up.finished {
                return done(Status::SoftError, "The local file was removed during sync.");
            } else {
                ctx.shared.another_sync_needed.set(true);
            }
        }
        // Check whether the file changed since discovery - this acts on the original file.
        let (size, mtime) = {
            let i = ctx.item.borrow();
            (i.size, i.modtime)
        };
        if !filesystem::verify_file_unchanged(&full_file_path, size, mtime) {
            ctx.shared.another_sync_needed.set(true);
            if !up.finished {
                return done(Status::SoftError, "Local file changed during sync.");
            }
        }
        if !up.finished {
            // Deletes an existing blacklist entry on successful chunk upload
            if ctx.item.borrow().has_blacklist_entry {
                ctx.shared.journal.wipe_error_blacklist_entry(&file);
                ctx.item.borrow_mut().has_blacklist_entry = false;
            }
            // Reset the error count on successful chunk upload
            let mut upload_info = ctx.shared.journal.get_upload_info(&file);
            upload_info.error_count = 0;
            ctx.shared.journal.set_upload_info(&file, &upload_info);
            ctx.shared.journal.commit("Upload info", true);
        }
    }

    // finishUpload: Finish with a MOVE
    up.finished = true;
    let destination = super::remote::clean_path(&format!(
        "{}{}",
        account.dav_url_path(),
        ctx.shared.full_remote_path(&up.file_to_upload.file)
    ));
    let mut headers = up.headers();
    // "If-Match applies to the source, but we are interested in comparing the etag of the destination
    if let Some(pos) = headers.iter().position(|(k, _)| *k == "If-Match") {
        let if_match = headers.remove(pos).1;
        if !if_match.is_empty() {
            let mut v = format!(
                "<{}> ([",
                nc_dav::account::to_percent_encoding_keep_slash(&destination)
            )
            .into_bytes();
            v.extend_from_slice(&if_match);
            v.extend_from_slice(b"])");
            headers.push(("If", v));
        }
    }
    if !up.transmission_checksum_header.is_empty() {
        headers.push(("OC-Checksum", up.transmission_checksum_header.clone()));
    }
    let file_size = up.file_to_upload.size;
    headers.push(("OC-Total-Length", file_size.to_string().into_bytes()));
    if let Some(h) = up.token_lock_if_header() {
        headers.retain(|(k, _)| *k != "If");
        headers.push(("If", h));
    }
    let mut opts = JobOptions::with_cancel(ctx.shared.hard_abort.child_token());
    adjust_last_job_timeout(&mut opts, file_size);
    ctx.shared.active_add(ctx.id, up.quick());
    let reply = nc_dav::jobs::move_(
        &account,
        &Target::Absolute(nc_dav::account::concat_url_path(
            &folder_path(transfer_id),
            "/.file",
        )),
        &destination,
        &headers,
        &opts,
    )
    .await;
    // slotMoveJobFinished
    ctx.shared.active_remove(ctx.id);
    {
        let mut i = ctx.item.borrow_mut();
        i.http_error_code = reply.http_status;
        i.response_time_stamp = reply.response_timestamp();
        i.request_id = reply.request_id.as_bytes().to_vec();
    }
    if reply.error != NetworkError::NoError {
        return up.common_error_handling(&reply);
    }
    let code = reply.http_status;
    if code == 202 {
        let path = reply.raw_header_str("OC-JobStatus-Location");
        if path.is_empty() {
            return done(Status::NormalError, "Poll URL missing");
        }
        return up.poll(&path).await;
    }
    if code != 201 && code != 204 {
        return done(
            Status::NormalError,
            format!("Unexpected return code from server ({code})"),
        );
    }
    let fid = reply.raw_header("OC-FileID").to_vec();
    if fid.is_empty() {
        log::warn!(target: "nextcloud.sync.propagator.upload.ng", "Server did not return a OC-FileID {file}");
        return done(Status::NormalError, "Missing File ID from server");
    }
    {
        let mut i = ctx.item.borrow_mut();
        // the old file id should only be empty for new files uploaded
        if !i.file_id.is_empty() && i.file_id != fid {
            log::warn!(target: "nextcloud.sync.propagator.upload.ng", "File ID changed! {:?} {:?}", String::from_utf8_lossy(&i.file_id), String::from_utf8_lossy(&fid));
        }
        i.file_id = fid;
        let dest = i.destination().to_owned();
        if let Ok(Some(old)) = ctx.shared.journal.get_file_record(dest.as_bytes())
            && old.is_valid()
            && old.etag != i.etag
        {
            i.update_lock_state_from_db_record(&old);
        }
        i.etag = get_etag_from_reply(&reply);
        if i.etag.is_empty() {
            log::warn!(target: "nextcloud.sync.propagator.upload.ng", "Server did not return an ETAG {file}");
            let msg = if i.is_directory() {
                "Folder is not accessible on the server."
            } else {
                "File is not accessible on the server."
            };
            return done(Status::NormalError, msg);
        }
    }
    up.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_helpers() {
        assert_eq!(qfileinfo_path("A/b/c"), "A/b");
        assert_eq!(qfileinfo_path("c"), ".");
        assert!(!file_is_still_changing(0, Duration::from_secs(2)));
        let now = jiff::Timestamp::now().as_second();
        assert!(file_is_still_changing(now, Duration::from_secs(2)));
        assert!(!file_is_still_changing(now, Duration::ZERO));
        let mut o = JobOptions {
            timeout: Duration::from_secs(300),
            ..JobOptions::default()
        };
        adjust_last_job_timeout(&mut o, 10);
        assert_eq!(o.timeout, Duration::from_secs(300));
        adjust_last_job_timeout(&mut o, 5_000_000_000);
        assert_eq!(o.timeout, Duration::from_millis(900_000));
    }
}
