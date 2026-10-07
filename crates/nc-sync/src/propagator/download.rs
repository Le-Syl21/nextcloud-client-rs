// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/propagatedownload.{h,cpp}` (GETFileJob and
// PropagateDownloadFile) of nextcloud/desktop v34.0.5, without the end-to-end
// encryption and virtual file paths.

//! `PropagateDownloadFile`: downloads a file into a temporary file next to
//! it (resumable), validates the checksum, handles conflicts and moves the
//! temporary file in place.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;

use http::{HeaderMap, HeaderValue, Method};
use nc_dav::{JobOptions, NetworkError, Reply, Target};
use nc_journal::checksums::{FailureReason, ValidateChecksumHeader, make_checksum_header};
use nc_journal::csync::ItemType;
use nc_journal::journal::{ConflictRecord, DownloadInfo};
use nc_journal::remote_permissions::Permission;

use tokio_util::sync::CancellationToken;

use super::bandwidth::BandwidthManager;
use super::{
    DiskSpaceResult, Done, JobCtx, Outcome, PropagatorEvent, classify_error,
    critical_free_space_limit, error_category_from_network_error, get_etag_from_reply,
};
use crate::filesystem::{self, FolderPermissions};
use crate::item::{ErrorCategory, Instruction, LockOwnerType, LockStatus, Status};

const LOG: &str = "nextcloud.sync.propagator.download";

/// `createDownloadTmpFileName(previous)`: `.<name>.~<random hex>` next to
/// the file, the name shortened so that the result fits 254 characters.
pub fn create_download_tmp_file_name(previous: &str) -> String {
    let (tmp_path, tmp_file_name) = match previous.rfind('/') {
        Some(i) => (&previous[..i], &previous[i + 1..]),
        None => ("", previous),
    };
    let overhead = 1 + 1 + 2 + 8; // slash dot dot-tilde ffffffff"
    let name_units: Vec<u16> = tmp_file_name.encode_utf16().collect();
    let space_for_file_name = (name_units.len() + overhead).min(254) - overhead;
    let short = String::from_utf16_lossy(&name_units[..space_for_file_name.min(name_units.len())]);
    let rand = fastrand::u32(0..0x7fff_ffff) % 0xFFFF_FFFF;
    if tmp_path.is_empty() {
        format!(".{short}.~{rand:x}")
    } else {
        format!("{tmp_path}/.{short}.~{rand:x}")
    }
}

/// `CustomDecompressedSafetyCheckThreshold`: added to the expected size
/// for the GET's `decompressedSafetyCheckThreshold`.
pub const CUSTOM_DECOMPRESSED_SAFETY_CHECK_THRESHOLD: i64 = 20 * 1024 * 1024;

const LOG_GET: &str = "nextcloud.sync.networkjob.get";

/// The device a [`GetFileJob`] writes the body to (the `QIODevice`).
pub trait GetFileDevice {
    /// `_device->write(data)`.
    fn write_data(&mut self, data: &[u8]) -> std::io::Result<()>;
    /// `_device->close(); _device->open(QIODevice::WriteOnly)`: start again
    /// from an empty device (the server ignored the range request).
    fn reopen_truncated(&mut self) -> std::io::Result<()>;
    fn flush_data(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl GetFileDevice for Vec<u8> {
    fn write_data(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.extend_from_slice(data);
        Ok(())
    }

    fn reopen_truncated(&mut self) -> std::io::Result<()> {
        self.clear();
        Ok(())
    }
}

/// The temporary file of a download.
struct TmpFile<'a> {
    file: &'a mut std::fs::File,
    path: &'a str,
}

impl GetFileDevice for TmpFile<'_> {
    fn write_data(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.file.write_all(data)
    }

    fn reopen_truncated(&mut self) -> std::io::Result<()> {
        *self.file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.path)?;
        Ok(())
    }

    fn flush_data(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// `GETFileJob`: a GET streamed into a device.
#[derive(Debug, Clone)]
pub struct GetFileJob {
    /// `makeDavUrl(path)` ([`Target::Dav`]) or `_directDownloadUrl`
    /// ([`Target::Url`]: sent without credentials).
    pub target: Target,
    /// `_headers`.
    pub headers: HeaderMap,
    /// `_expectedEtagForResume`.
    pub expected_etag_for_resume: Vec<u8>,
    /// `_resumeStart`.
    pub resume_start: i64,
    /// `_expectedContentLength` (-1: not checked).
    pub expected_content_length: i64,
    /// `_decompressionThresholdBase`.
    pub decompression_threshold_base: i64,
    /// `setBandwidthManager()`.
    pub bandwidth_manager: Option<Arc<BandwidthManager>>,
    /// Aborts the request (`reply()->abort()`).
    pub cancel: Option<CancellationToken>,
}

/// A finished [`GetFileJob`].
#[derive(Debug, Clone, Default)]
pub struct GetFileResult {
    pub reply: Reply,
    /// `_errorString` / `_errorStatus` set by the job itself.
    pub error_string: String,
    pub error_status: Status,
    /// `etag()` (empty for a direct download).
    pub etag: Vec<u8>,
    /// `lastModified()`.
    pub last_modified: i64,
    /// `resumeStart()` (0 after a server ignored the range).
    pub resume_start: i64,
    /// `contentLength()` (-1 when unknown).
    pub content_length: i64,
    /// `finishedSignal` was emitted. A reply that failed while its body was
    /// being decoded never emits it upstream.
    pub finished_signal: bool,
}

impl GetFileResult {
    /// `GETFileJob::errorString()`.
    pub fn error_string(&self) -> String {
        if !self.error_string.is_empty() {
            return self.error_string.clone();
        }
        self.reply.error_string()
    }
}

/// Interval of `QNetworkReply::downloadProgress` signals.
const PROGRESS_SIGNAL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Reads the next chunk of the body; `Err` ends the reply with an error.
async fn next_body_chunk(
    body: &mut nc_dav::BodyStream,
    opts: &JobOptions,
) -> Result<Option<nc_dav::Bytes>, nc_dav::TransportError> {
    nc_dav::jobs::next_chunk(body, opts).await
}

impl GetFileJob {
    /// `GETFileJob(account, path, device, headers, expectedEtagForResume, resumeStart)`.
    pub fn new(
        target: Target,
        headers: HeaderMap,
        expected_etag_for_resume: &[u8],
        resume_start: i64,
    ) -> Self {
        Self {
            target,
            headers,
            expected_etag_for_resume: expected_etag_for_resume.to_vec(),
            resume_start,
            expected_content_length: -1,
            decompression_threshold_base: 0,
            bandwidth_manager: None,
            cancel: None,
        }
    }

    fn is_direct_download(&self) -> bool {
        matches!(self.target, Target::Url(_))
    }

    /// `start()` until `finishedSignal`. `progress(received, total)` is
    /// `downloadProgress` (bytes of this reply, total -1 when unknown).
    pub async fn run(
        self,
        account: &nc_dav::Account,
        device: &mut dyn GetFileDevice,
        progress: &mut dyn FnMut(i64, i64),
    ) -> GetFileResult {
        let mut headers = self.headers.clone();
        let resume_start = self.resume_start;
        if resume_start > 0 {
            if let Ok(v) = HeaderValue::from_str(&format!("bytes={resume_start}-")) {
                headers.insert("Range", v);
            }
            headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
            log::debug!(target: LOG_GET, "Retry with range {:?}", headers.get("Range"));
        }
        let mut opts = match &self.cancel {
            Some(c) => JobOptions::with_cancel(c.clone()),
            None => JobOptions::default(),
        };
        opts.decompressed_safety_check_threshold =
            Some(self.decompression_threshold_base + CUSTOM_DECOMPRESSED_SAFETY_CHECK_THRESHOLD);
        // Use direct URL without credentials
        opts.dont_add_credentials = self.is_direct_download();
        let mut result = GetFileResult {
            resume_start,
            content_length: -1,
            ..GetFileResult::default()
        };
        // registerDownloadJob (unregistered when dropped)
        let registration = self
            .bandwidth_manager
            .as_ref()
            .map(|bwm| bwm.register_download_job());
        let mut progress_choke = std::time::Instant::now();
        let streaming = match nc_dav::jobs::send_streaming(
            account,
            Method::GET,
            &self.target,
            headers,
            nc_dav::Body::empty(),
            &opts,
        )
        .await
        {
            Ok(s) => s,
            Err(reply) => {
                if reply.timed_out {
                    result.error_string = "Connection Timeout".to_owned();
                    result.error_status = Status::FatalError;
                }
                result.reply = reply;
                result.finished_signal = true;
                return result;
            }
        };
        let mut reply = streaming.reply;
        let mut body = streaming.body;
        let http_status = reply.http_status;
        let total = {
            let v = reply.raw_header_str("Content-Length");
            v.trim().parse::<i64>().unwrap_or(-1)
        };
        // slotMetaDataChanged
        let mut save_body_to_file = false;
        if http_status / 100 == 2 && reply.is_ok() {
            let mut abort_with: Option<String> = None;
            result.etag = get_etag_from_reply(&reply);
            if self.is_direct_download() && !result.etag.is_empty() {
                log::info!(target: LOG_GET, "Direct download used, ignoring server ETag {}", String::from_utf8_lossy(&result.etag));
                result.etag.clear(); // reset received ETag
            } else if self.is_direct_download() {
                // All fine, ETag empty and directDownloadUrl used
            } else if result.etag.is_empty() {
                log::warn!(target: LOG_GET, "No E-Tag reply by server, considering it invalid");
                abort_with = Some("No E-Tag received from server, check Proxy/Gateway".to_owned());
            } else if !self.expected_etag_for_resume.is_empty()
                && self.expected_etag_for_resume != result.etag
            {
                log::warn!(target: LOG_GET, "We received a different E-Tag for resuming!");
                abort_with = Some(
                    "We received a different E-Tag for resuming. Retrying next time.".to_owned(),
                );
            }
            if abort_with.is_none() {
                let content_length = reply.raw_header_str("Content-Length");
                if let Ok(len) = content_length.trim().parse::<i64>() {
                    result.content_length = len;
                    if self.expected_content_length != -1 && len != self.expected_content_length {
                        log::warn!(target: LOG_GET, "We received a different content length than expected! {} vs {len}", self.expected_content_length);
                        abort_with =
                            Some("We received an unexpected download Content-Length.".to_owned());
                    }
                }
            }
            if abort_with.is_none() {
                let mut start = 0i64;
                let ranges = reply.raw_header_str("Content-Range");
                if !ranges.is_empty()
                    && let Some(rest) = ranges.find("bytes ").map(|i| &ranges[i + 6..])
                {
                    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                    if rest[digits.len()..].starts_with('-') {
                        start = digits.parse().unwrap_or(0);
                    }
                }
                if start != resume_start {
                    log::warn!(target: LOG_GET, "Wrong content-range: {ranges} while expecting start was {resume_start}");
                    if ranges.is_empty() {
                        // device doesn't support range, just try again from scratch
                        match device.reopen_truncated() {
                            Ok(()) => result.resume_start = 0,
                            Err(e) => abort_with = Some(e.to_string()),
                        }
                    } else {
                        abort_with = Some("Server returned wrong content-range".to_owned());
                    }
                }
            }
            if let Some(msg) = abort_with {
                // reply()->abort()
                result.error_string = msg;
                result.error_status = Status::NormalError;
                reply.error = NetworkError::OperationCanceledError;
                progress(0, total);
                result.reply = reply;
                result.finished_signal = true;
                return result;
            }
            let last_modified = reply.raw_header_str("Last-Modified");
            if !last_modified.is_empty()
                && let Ok(t) = httpdate::parse_http_date(&last_modified)
                && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
            {
                result.last_modified = d.as_secs() as i64;
            }
            save_body_to_file = true;
        }
        // slotReadyRead
        let mut received: i64 = 0;
        let mut error_body = Vec::new();
        result.finished_signal = true;
        'read: loop {
            match next_body_chunk(&mut body, &opts).await {
                Ok(Some(chunk)) => {
                    received += chunk.len() as i64;
                    if progress_choke.elapsed() >= PROGRESS_SIGNAL_INTERVAL {
                        progress_choke = std::time::Instant::now();
                        progress(received, total);
                    }
                    if !save_body_to_file {
                        error_body.extend_from_slice(&chunk);
                        continue;
                    }
                    let mut rest: &[u8] = &chunk;
                    while !rest.is_empty() {
                        let n = match &registration {
                            None => rest.len(),
                            Some(reg) => {
                                let want = rest.len().min(8 * 1024);
                                let acquire = reg.client().acquire(want);
                                match &opts.cancel {
                                    Some(token) => tokio::select! {
                                        biased;
                                        _ = token.cancelled() => {
                                            reply.error = NetworkError::OperationCanceledError;
                                            break 'read;
                                        }
                                        n = acquire => n,
                                    },
                                    None => acquire.await,
                                }
                            }
                        };
                        if let Err(e) = device.write_data(&rest[..n]) {
                            log::warn!(target: LOG_GET, "Error while writing to file {e}");
                            result.error_string = e.to_string();
                            result.error_status = Status::NormalError;
                            reply.error = NetworkError::OperationCanceledError;
                            break 'read;
                        }
                        rest = &rest[n..];
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let timed_out = matches!(e, nc_dav::TransportError::Timeout);
                    reply.error = match e {
                        nc_dav::TransportError::Timeout | nc_dav::TransportError::Canceled => {
                            NetworkError::OperationCanceledError
                        }
                        nc_dav::TransportError::UnknownContent(_) => {
                            // Reading from the reply failed: upstream's
                            // job never emits finishedSignal then.
                            result.finished_signal = false;
                            if save_body_to_file {
                                result.error_status = Status::NormalError;
                            }
                            NetworkError::UnknownContentError
                        }
                        _ => NetworkError::RemoteHostClosedError,
                    };
                    if reply.error == NetworkError::UnknownContentError && save_body_to_file {
                        result.error_string = reply.error_string();
                        log::warn!(target: LOG_GET, "Error while reading from device: {}", result.error_string);
                    }
                    if timed_out {
                        log::warn!(target: LOG_GET, "Timeout {}", reply.url);
                        reply.timed_out = true;
                        result.error_string = "Connection Timeout".to_owned();
                        result.error_status = Status::FatalError;
                    }
                    break;
                }
            }
        }
        let _ = device.flush_data();
        // The last downloadProgress of a finished reply.
        progress(received, if total == -1 { received } else { total });
        drop(registration);
        if result.finished_signal {
            log::info!(target: LOG_GET, "GET of {} FINISHED WITH STATUS {:?} {} {}", reply.url, reply.error, reply.raw_header_str("Content-Range"), reply.raw_header_str("Content-Length"));
        }
        reply.body = error_body.into();
        result.reply = reply;
        result
    }
}

/// Restores the parent folder permissions on drop (`_needParentFolderRestorePermissions`).
struct ParentPermissions {
    path: String,
    restore: bool,
}

impl ParentPermissions {
    /// `makeParentFolderModifiable(fileName)`.
    fn make_modifiable(&mut self, ctx: &JobCtx, file_name: &str) {
        self.path = filesystem::parent_dir(file_name);
        if filesystem::is_folder_read_only(&self.path) {
            filesystem::set_folder_permissions(&self.path, FolderPermissions::ReadWrite);
            ctx.shared
                .emit(PropagatorEvent::TouchedFile(self.path.clone()));
            self.restore = true;
        }
    }

    fn restore_now(&mut self, ctx: &JobCtx) {
        if self.restore {
            filesystem::set_folder_permissions(&self.path, FolderPermissions::ReadOnly);
            ctx.shared
                .emit(PropagatorEvent::TouchedFile(self.path.clone()));
            self.restore = false;
        }
    }
}

fn done(status: Status, msg: impl Into<String>, cat: ErrorCategory) -> Outcome {
    Some(Done::new(status, msg, cat))
}

/// Runs the download and restores the parent folder permissions
/// (`PropagateDownloadFile::done`).
pub(super) async fn download(ctx: JobCtx) -> Outcome {
    let mut parent = ParentPermissions {
        path: String::new(),
        restore: false,
    };
    let outcome = download_inner(&ctx, &mut parent).await;
    parent.restore_now(&ctx);
    ctx.shared.committed_disk_space.borrow_mut().remove(&ctx.id);
    outcome
}

/// `PropagateDownloadFile::start` .. `updateMetadata`.
async fn download_inner(ctx: &JobCtx, parent: &mut ParentPermissions) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    // (E2EE: the parent is never encrypted for this client.)
    // startAfterIsEncryptedIsChecked
    {
        let mut i = ctx.item.borrow_mut();
        if i.item_type == ItemType::VirtualFile {
            log::warn!(target: LOG, "ignored virtual file type of {}", i.file);
            i.item_type = ItemType::File;
        }
    }
    if ctx.delete_existing
        && let Some(outcome) = delete_existing_folder(ctx)
    {
        return outcome;
    }
    // If we have a conflict where size of the file is unchanged,
    // compare the remote checksum to the local one.
    // Maybe it's not a real conflict and no download is necessary!
    // If the hashes are collision safe and identical, we assume the content is too.
    // For weak checksums, we only do that if the mtimes are also identical.
    let (instruction, size, previous_size, checksum_header, modtime, previous_modtime, file) = {
        let i = ctx.item.borrow();
        (
            i.instruction,
            i.size,
            i.previous_size,
            i.checksum_header.clone(),
            i.modtime,
            i.previous_modtime,
            i.file.clone(),
        )
    };
    if modtime <= 0 {
        log::warn!(target: LOG, "invalid modified time {file} {modtime}");
    }
    let is_collision_safe =
        checksum_header.starts_with(b"SHA") || checksum_header.starts_with(b"MD5:");
    if instruction == Instruction::Conflict
        && size == previous_size
        && !checksum_header.is_empty()
        && (is_collision_safe || modtime == previous_modtime)
    {
        ctx.shared
            .active_add(ctx.id, size < super::Shared::small_file_size());
        let ty = nc_journal::checksums::parse_checksum_header_type(&checksum_header);
        let mut compute = nc_journal::checksums::ComputeChecksum::new();
        compute.set_checksum_type(&ty);
        let (ctype, csum) = compute.start(std::path::Path::new(&ctx.shared.full_local_path(&file)));
        // conflictChecksumComputed
        ctx.shared.active_remove(ctx.id);
        if make_checksum_header(&ctype, &csum) == checksum_header {
            // No download necessary, just update fs and journal metadata
            let fname = ctx.shared.full_local_path(&file);
            if modtime <= 0 {
                log::warn!(target: LOG, "invalid modified time {file} {modtime}");
                return None;
            }
            if modtime != previous_modtime {
                filesystem::set_mod_time(&fname, modtime);
                ctx.shared.emit(PropagatorEvent::TouchedFile(fname.clone()));
            }
            let new_modtime = filesystem::get_mod_time(&fname);
            ctx.item.borrow_mut().modtime = new_modtime;
            if new_modtime <= 0 {
                log::warn!(target: LOG, "invalid modified time {file} {new_modtime}");
                return None;
            }
            return update_metadata(ctx, false);
        }
    }
    start_download(ctx, parent).await
}

/// `deleteExistingFolder()`: `Some(outcome)` when it failed (`done()` was called).
fn delete_existing_folder(ctx: &JobCtx) -> Option<Outcome> {
    let file = ctx.item.borrow().file.clone();
    let existing_dir = ctx.shared.full_local_path(&file);
    if !filesystem::is_dir(&existing_dir) {
        return None;
    }
    // Delete the directory if it is empty!
    let empty = std::fs::read_dir(&existing_dir)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(false);
    if empty && std::fs::remove_dir(&existing_dir).is_ok() {
        return None;
    }
    // on error, just try to move it away...
    if let Err(error) = ctx
        .shared
        .create_conflict(&ctx.item, ctx.associated_composite)
    {
        return Some(done(
            Status::NormalError,
            error,
            ErrorCategory::GenericError,
        ));
    }
    None
}

/// `startDownload` + the `GETFileJob` + `slotGetFinished` and the rest of
/// the chain.
async fn start_download(ctx: &JobCtx, parent: &mut ParentPermissions) -> Outcome {
    if ctx.shared.abort_requested.get() {
        return None;
    }
    let file = ctx.item.borrow().file.clone();
    // do a klaas' case clash check.
    if ctx.shared.local_file_name_clash(&file) {
        ctx.item.borrow_mut().instruction = Instruction::CaseClashConflict;
    }
    ctx.shared.report_progress(&ctx.item.borrow().clone(), 0);
    let (etag, size) = {
        let i = ctx.item.borrow();
        (i.etag.clone(), i.size)
    };
    let mut tmp_file_name = String::new();
    let mut expected_etag_for_resume = Vec::new();
    let progress_info = ctx.shared.journal.get_download_info(&file);
    if progress_info.valid {
        // if the etag has changed meanwhile, remove the already downloaded part.
        if progress_info.etag != etag {
            let fname = ctx.shared.full_local_path(&progress_info.tmpfile);
            if filesystem::file_exists(&fname) {
                let _ = filesystem::remove(&fname);
            }
            ctx.shared
                .journal
                .set_download_info(&file, &DownloadInfo::default());
        } else {
            tmp_file_name = progress_info.tmpfile.clone();
            expected_etag_for_resume = progress_info.etag.clone();
        }
    }
    if tmp_file_name.is_empty() {
        tmp_file_name = create_download_tmp_file_name(&file);
    }
    let tmp_path = ctx.shared.full_local_path(&tmp_file_name);
    parent.make_modifiable(ctx, &tmp_path);
    let resume_start = std::fs::metadata(&tmp_path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    if resume_start > 0 && resume_start == size {
        // The whole file is already there.
        return download_finished(ctx, &tmp_path, parent, false, ConflictRecord::default());
    }
    // Can't open(Append) read-only files, make sure to make
    // file writable if it exists.
    if filesystem::file_exists(&tmp_path) {
        filesystem::set_file_read_only(&tmp_path, false);
    }
    let mut tmp_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&tmp_path)
    {
        Ok(f) => f,
        Err(e) => {
            log::warn!(target: LOG, "could not open temporary file {tmp_path}");
            return done(
                Status::NormalError,
                e.to_string(),
                ErrorCategory::GenericError,
            );
        }
    };
    // Hide temporary after creation
    filesystem::set_file_hidden(&tmp_path, true);
    // If there's not enough space to fully download this file, stop.
    ctx.shared
        .committed_disk_space
        .borrow_mut()
        .insert(ctx.id, (size - resume_start).clamp(0, size.max(0)));
    let committed: i64 = ctx.shared.committed_disk_space.borrow().values().sum();
    match ctx.shared.disk_space_check(committed) {
        DiskSpaceResult::Ok => {}
        r => {
            let outcome = if r == DiskSpaceResult::Failure {
                // Using DetailError here will make the error not pop up in the account
                // tab: instead we'll generate a general "disk space low" message and show
                // these detail errors only in the error view.
                ctx.shared.emit(PropagatorEvent::InsufficientLocalStorage);
                done(
                    Status::DetailError,
                    "The download would reduce free local disk space below the limit",
                    ErrorCategory::GenericError,
                )
            } else {
                done(
                    Status::FatalError,
                    format!(
                        "Free space on disk is less than {}",
                        crate::utility::octets_to_string(critical_free_space_limit())
                    ),
                    ErrorCategory::GenericError,
                )
            };
            // Remove the temporary, if empty.
            if resume_start == 0 {
                drop(tmp_file);
                let _ = std::fs::remove_file(&tmp_path);
            }
            return outcome;
        }
    }
    ctx.shared.journal.set_download_info(
        &file,
        &DownloadInfo {
            etag: etag.clone(),
            tmpfile: tmp_file_name.clone(),
            valid: true,
            error_count: 0,
        },
    );
    ctx.shared.journal.commit("download file start", true);

    ctx.shared
        .active_add(ctx.id, size < super::Shared::small_file_size());
    let direct_download_url = ctx.item.borrow().direct_download_url.clone();
    let mut job = if direct_download_url.is_empty() {
        // Normal job, download from oC instance
        GetFileJob::new(
            Target::Dav(ctx.shared.full_remote_path(&file)),
            HeaderMap::new(),
            &expected_etag_for_resume,
            resume_start,
        )
    } else {
        // We were provided a direct URL, use that one
        log::info!(target: LOG, "directDownloadUrl given for {file} {direct_download_url}");
        let mut headers = HeaderMap::new();
        let cookies = ctx.item.borrow().direct_download_cookies.clone();
        if !cookies.is_empty()
            && let Ok(v) = HeaderValue::from_str(&cookies)
        {
            headers.insert(http::header::COOKIE, v);
        }
        GetFileJob::new(
            Target::Url(direct_download_url.clone()),
            headers,
            &expected_etag_for_resume,
            resume_start,
        )
    };
    if size >= 0 {
        job.decompression_threshold_base = size;
    }
    job.bandwidth_manager = Some(ctx.shared.bandwidth.clone());
    job.cancel = Some(ctx.shared.soft_abort.child_token());
    let get = {
        let mut device = TmpFile {
            file: &mut tmp_file,
            path: &tmp_path,
        };
        // slotDownloadProgress
        let mut on_progress = |received: i64, _total: i64| {
            ctx.shared.committed_disk_space.borrow_mut().insert(
                ctx.id,
                (size - resume_start - received).clamp(0, size.max(0)),
            );
            let item = ctx.item.borrow().clone();
            ctx.shared.report_progress(&item, resume_start + received);
        };
        job.run(&ctx.shared.account, &mut device, &mut on_progress)
            .await
    };
    drop(tmp_file);
    // slotGetFinished
    ctx.shared.active_remove(ctx.id);
    {
        let mut i = ctx.item.borrow_mut();
        i.http_error_code = get.reply.http_status;
        i.request_id = get.reply.request_id.as_bytes().to_vec();
    }
    let err = get.reply.error;
    let http_code = get.reply.http_status;
    if err != NetworkError::NoError {
        // If we sent a 'Range' header and get 416 back, we want to retry without the header.
        let bad_range_header = get.resume_start > 0 && http_code == 416;
        if bad_range_header {
            log::warn!(target: LOG, "server replied 416 to our range request, trying again without");
            ctx.shared.another_sync_needed.set(true);
        }
        // Getting a 404 probably means that the file was deleted on the server.
        let file_not_found = http_code == 404;
        if file_not_found {
            log::warn!(target: LOG, "server replied 404, assuming file was deleted");
        }
        // Don't keep the temporary file if it is empty or we
        // used a bad range header or the file's not on the server anymore.
        let tmp_size = std::fs::metadata(&tmp_path).map(|m| m.len()).ok();
        if let Some(sz) = tmp_size
            && (sz == 0 || bad_range_header || file_not_found)
        {
            let _ = filesystem::remove(&tmp_path);
            ctx.shared
                .journal
                .set_download_info(&file, &DownloadInfo::default());
        }
        if !direct_download_url.is_empty() && err != NetworkError::OperationCanceledError {
            // If this was with a direct download, retry without direct download
            log::warn!(target: LOG, "Direct download of {direct_download_url} failed. Retrying through owncloud.");
            ctx.item.borrow_mut().direct_download_url.clear();
            return Box::pin(download_inner(ctx, parent)).await;
        }
        let mut error_status = get.error_status;
        let mut error_string_override = None;
        if bad_range_header {
            // Can't do this in classifyError() because 416 without a
            // Range header should result in NormalError.
            error_status = Status::SoftError;
        } else if file_not_found {
            error_string_override = Some("File was deleted from server".to_owned());
            error_status = Status::SoftError;
            // As a precaution against bugs that cause our database and the
            // reality on the server to diverge, rediscover this folder on the
            // next sync run.
            ctx.shared
                .journal
                .schedule_path_for_remote_discovery(file.as_bytes());
        }
        let error_string = if let Some(s) = error_string_override {
            s
        } else if http_code >= 400 {
            if get.error_string.is_empty() {
                get.reply.error_string_parsing_body()
            } else {
                get.error_string.clone()
            }
        } else {
            get.error_string()
        };
        let status = if error_status == Status::NoStatus {
            classify_error(
                err,
                http_code,
                Some(&ctx.shared.another_sync_needed),
                &get.reply.body,
            )
        } else {
            error_status
        };
        return done(status, error_string, error_category_from_network_error(err));
    }
    {
        let mut i = ctx.item.borrow_mut();
        i.response_time_stamp = get.reply.response_timestamp();
        if !get.etag.is_empty() {
            i.etag = nc_dav::xml::parse_etag(&get.etag);
        }
        if get.last_modified != 0 {
            // It is possible that the file was modified on the server since we did the discovery phase
            // so make sure we have the up-to-date time
            i.modtime = get.last_modified;
            if i.modtime <= 0 {
                log::warn!(target: LOG, "invalid modified time {} {}", i.file, i.modtime);
            }
        }
    }
    // Check that the size of the GET reply matches the file size.
    let size_header = get.reply.raw_header("Content-Length");
    let mut has_size_header = !size_header.is_empty();
    let mut body_size = crate::utility::qstring_to_long_long(&String::from_utf8_lossy(size_header));
    // Qt removes the content-length header for transparently decompressed HTTP1 replies
    // but not for HTTP2 or SPDY replies. For these it remains and contains the size
    // of the compressed data. See QTBUG-73364.
    let content_encoding = get
        .reply
        .raw_header("content-encoding")
        .to_ascii_lowercase();
    if (content_encoding == b"gzip" || content_encoding == b"deflate") && get.reply.http2_was_used {
        body_size = 0;
        has_size_header = false;
    }
    let tmp_size = std::fs::metadata(&tmp_path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    if has_size_header && tmp_size > 0 && body_size == 0 {
        // Strange bug with broken webserver or webfirewall
        let _ = filesystem::remove(&tmp_path);
        return done(
            Status::SoftError,
            "Broken webserver returning empty content length for non-empty file on resume",
            ErrorCategory::GenericError,
        );
    }
    if body_size > 0 && body_size != tmp_size - get.resume_start {
        ctx.shared.another_sync_needed.set(true);
        return done(
            Status::SoftError,
            "The file could not be downloaded completely.",
            ErrorCategory::GenericError,
        );
    }
    if tmp_size == 0 && size > 0 {
        let _ = filesystem::remove(&tmp_path);
        return done(
            Status::NormalError,
            format!(
                "The downloaded file is empty, but the server said it should have been {}.",
                crate::utility::octets_to_string(size)
            ),
            ErrorCategory::GenericError,
        );
    }
    // Did the file come with conflict headers? If so, store them now!
    let mut conflict_record = ConflictRecord::default();
    if get.reply.raw_header("OC-Conflict") == b"1" {
        conflict_record.path = file.as_bytes().to_vec();
        conflict_record.initial_base_path =
            get.reply.raw_header("OC-ConflictInitialBasePath").to_vec();
        conflict_record.base_file_id = get.reply.raw_header("OC-ConflictBaseFileId").to_vec();
        conflict_record.base_etag = get.reply.raw_header("OC-ConflictBaseEtag").to_vec();
        let mtime_header = get.reply.raw_header("OC-ConflictBaseMtime");
        if !mtime_header.is_empty() {
            conflict_record.base_modtime =
                crate::utility::qstring_to_long_long(&String::from_utf8_lossy(mtime_header));
        }
    }
    // Do checksum validation for the download.
    let mut checksum_header =
        nc_journal::checksums::find_best_checksum(get.reply.raw_header("OC-Checksum"));
    let content_md5 = get.reply.raw_header("Content-MD5");
    if checksum_header.is_empty() && !content_md5.is_empty() {
        checksum_header = [b"MD5:".as_slice(), content_md5].concat();
    }
    let mut validator = ValidateChecksumHeader::new();
    let (ctype, csum) = match validator.start(std::path::Path::new(&tmp_path), &checksum_header) {
        Ok(v) => v,
        Err(failure) => {
            // slotChecksumFail
            match checksum_recalculate(ctx, &file, &failure).await {
                Some(v) => v,
                None => {
                    // checksumValidateFailedAbortDownload
                    let _ = filesystem::remove(&tmp_path);
                    ctx.shared.another_sync_needed.set(true);
                    return done(
                        Status::SoftError,
                        failure.message,
                        ErrorCategory::GenericError,
                    );
                }
            }
        }
    };
    // transmissionChecksumValidated
    let content_type = ctx
        .shared
        .account
        .capabilities()
        .preferred_upload_checksum_type();
    let (ctype, csum) = if content_type == ctype || content_type.is_empty() {
        (ctype, csum)
    } else {
        // Compute the content checksum.
        let mut compute = nc_journal::checksums::ComputeChecksum::new();
        compute.set_checksum_type(&content_type);
        compute.start(std::path::Path::new(&tmp_path))
    };
    // contentChecksumComputed
    ctx.item.borrow_mut().checksum_header = make_checksum_header(&ctype, &csum);
    let local_file_path = ctx.shared.full_local_path(&file);
    let (instruction, item_modtime, item_etag, item_type, item_checksum) = {
        let i = ctx.item.borrow();
        (
            i.instruction,
            i.modtime,
            i.etag.clone(),
            i.item_type,
            i.checksum_header.clone(),
        )
    };
    if instruction != Instruction::Conflict
        && filesystem::file_exists(&local_file_path)
        && item_type == ItemType::File
        && let Ok(Some(record)) = ctx.shared.journal.get_file_record(file.as_bytes())
        && record.is_valid()
        && record.modtime == item_modtime
        && record.etag != item_etag
    {
        let mut compute = nc_journal::checksums::ComputeChecksum::new();
        compute.set_checksum_type(&ctype);
        let (lt, lc) = compute.start(std::path::Path::new(&local_file_path));
        // localFileContentChecksumComputed
        if item_checksum == make_checksum_header(&lt, &lc) {
            let _ = filesystem::remove(&tmp_path);
            return update_metadata(ctx, false);
        }
    }
    // finalizeDownload (no E2EE)
    download_finished(ctx, &tmp_path, parent, true, conflict_record)
}

/// `slotChecksumFail` with a server-side checksum recalculation request:
/// `Some((type, checksum))` when the recalculated server checksum matches.
async fn checksum_recalculate(
    ctx: &JobCtx,
    file: &str,
    failure: &nc_journal::checksums::ValidationFailure,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if failure.reason != FailureReason::ChecksumMismatch
        || !ctx
            .shared
            .account
            .is_checksum_recalculate_request_supported()
    {
        return None;
    }
    let calculated_header = [
        failure.calculated_checksum_type.as_slice(),
        b":",
        failure.calculated_checksum.as_slice(),
    ]
    .concat();
    log::warn!(target: LOG, "Checksum validation has failed for file: {file} Requesting checksum recalculation on the server...");
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_bytes(&failure.calculated_checksum_type) {
        headers.insert("X-Recalculate-Hash", v);
    }
    let reply = nc_dav::jobs::send(
        &ctx.shared.account,
        Method::PATCH,
        &Target::Dav(ctx.shared.full_remote_path(file)),
        headers,
        nc_dav::Body::empty(),
        &JobOptions::with_cancel(ctx.shared.soft_abort.child_token()),
    )
    .await;
    // processChecksumRecalculate
    if !reply.is_ok() {
        log::error!(target: LOG, "Checksum recalculation has failed for file: {} Recalculation request has finished with error: {}", reply.url, reply.error_string());
        return None;
    }
    let new_header = reply.raw_header("OC-Checksum").to_vec();
    if new_header == calculated_header {
        let parts: Vec<&[u8]> = new_header.split(|c| *c == b':').collect();
        if parts.len() > 1 {
            return Some((parts[0].to_vec(), parts[parts.len() - 1].to_vec()));
        }
    }
    log::error!(target: LOG, "Checksum recalculation has failed for file: {} OC-Checksum received is: {}", reply.url, String::from_utf8_lossy(&new_header));
    None
}

/// `downloadFinished()`.
fn download_finished(
    ctx: &JobCtx,
    tmp_path: &str,
    parent: &mut ParentPermissions,
    _validated: bool,
    conflict_record: ConflictRecord,
) -> Outcome {
    let file = ctx.item.borrow().file.clone();
    let filename = ctx.shared.full_local_path(&file);
    let modtime = ctx.item.borrow().modtime;
    if modtime <= 0 {
        let _ = filesystem::remove(tmp_path);
        return done(
            Status::NormalError,
            format!("File {file} has invalid modified time reported by server. Do not save it."),
            ErrorCategory::GenericError,
        );
    }
    filesystem::set_mod_time(tmp_path, modtime);
    // We need to fetch the time again because some file systems such as FAT have worse than a second
    // Accuracy, and we really need the time from the file system. (#3103)
    let modtime = filesystem::get_mod_time(tmp_path);
    ctx.item.borrow_mut().modtime = modtime;
    if modtime <= 0 {
        let _ = filesystem::remove(tmp_path);
        return done(
            Status::NormalError,
            format!("File {file} has invalid modified time reported by server. Do not save it."),
            ErrorCategory::GenericError,
        );
    }
    if ctx.shared.local_file_name_clash(&file) {
        ctx.item.borrow_mut().instruction = Instruction::CaseClashConflict;
    }
    let instruction = ctx.item.borrow().instruction;
    let mut previous_file_exists =
        filesystem::file_exists(&filename) && instruction != Instruction::CaseClashConflict;
    if previous_file_exists {
        // Preserve the existing file permissions.
        if let (Ok(existing), Ok(tmp)) = (std::fs::metadata(&filename), std::fs::metadata(tmp_path))
        {
            if existing.permissions().mode() & 0o7777 != tmp.permissions().mode() & 0o7777 {
                let _ = std::fs::set_permissions(
                    tmp_path,
                    std::fs::Permissions::from_mode(existing.permissions().mode()),
                );
            }
            // preserveGroupOwnership
            if let Err(e) = std::os::unix::fs::chown(tmp_path, None, Some(existing.gid())) {
                log::warn!(target: LOG, "preserveGroupOwnership: chown error {e}: setting group {} failed on file {tmp_path}", existing.gid());
            }
        }
    }
    {
        let i = ctx.item.borrow();
        if i.locked == LockStatus::LockedItem
            && (i.lock_owner_type != LockOwnerType::UserLock
                || i.lock_owner_id != ctx.shared.account.dav_user())
        {
            filesystem::set_file_read_only(tmp_path, true);
        } else {
            filesystem::set_file_read_only_weak(
                tmp_path,
                !i.remote_perm.is_null() && !i.remote_perm.has_permission(Permission::CanWrite),
            );
        }
    }
    let is_conflict = (instruction == Instruction::Conflict
        && (filesystem::is_dir(&filename) || !filesystem::file_equals(&filename, tmp_path)))
        || instruction == Instruction::CaseClashConflict;
    if is_conflict {
        if instruction == Instruction::CaseClashConflict {
            return match ctx.shared.create_case_clash_conflict(&ctx.item, tmp_path) {
                Some(e) => done(Status::SoftError, e, ErrorCategory::GenericError),
                None => done(
                    Status::FileNameClash,
                    format!("File {file} downloaded but it resulted in a local file name clash!"),
                    ErrorCategory::GenericError,
                ),
            };
        }
        if let Err(error) = ctx
            .shared
            .create_conflict(&ctx.item, ctx.associated_composite)
        {
            return done(Status::SoftError, error, ErrorCategory::GenericError);
        }
        previous_file_exists = false;
    }
    let is_virtual_download = ctx.item.borrow().item_type == ItemType::VirtualFileDownload;
    if previous_file_exists && !is_virtual_download {
        // Check whether the existing file has changed since the discovery
        // phase by comparing size and mtime to the previous values.
        let (expected_size, expected_mtime) = {
            let i = ctx.item.borrow();
            (i.previous_size, i.previous_modtime)
        };
        if !filesystem::verify_file_unchanged(&filename, expected_size, expected_mtime) {
            ctx.shared.another_sync_needed.set(true);
            return done(
                Status::SoftError,
                "File has changed since discovery",
                ErrorCategory::GenericError,
            );
        }
    }
    ctx.shared
        .emit(PropagatorEvent::TouchedFile(filename.clone()));
    if let Err(error) = filesystem::unchecked_rename_replace(tmp_path, &filename) {
        log::warn!(target: LOG, "Rename failed: {tmp_path} => {filename}");
        // If the file is locked, we want to retry this sync when it
        // becomes available again, otherwise try again directly
        if filesystem::is_file_locked(&filename) {
            ctx.shared.emit(PropagatorEvent::SeenLockedFile(filename));
        } else {
            ctx.shared.another_sync_needed.set(true);
        }
        return done(Status::SoftError, error, ErrorCategory::GenericError);
    }
    filesystem::set_file_hidden(&filename, false);
    parent.restore_now(ctx);
    // Maybe we downloaded a newer version of the file than we thought we would...
    // Get up to date information for the journal.
    ctx.item.borrow_mut().size = filesystem::get_size(&filename);
    // Maybe what we downloaded was a conflict file? If so, set a conflict record.
    if conflict_record.is_valid() {
        ctx.shared.journal.set_conflict_record(&conflict_record);
    }
    update_metadata(ctx, is_conflict)
}

/// `PropagateDownloadFile::updateMetadata(isConflict)`.
fn update_metadata(ctx: &JobCtx, is_conflict: bool) -> Outcome {
    let file = ctx.item.borrow().file.clone();
    let fname = ctx.shared.full_local_path(&file);
    if let Err(e) = ctx.shared.update_metadata(&ctx.item.borrow()) {
        return done(
            Status::FatalError,
            format!("Error updating metadata: {e}"),
            ErrorCategory::GenericError,
        );
    }
    // Upstream quirk kept: for unencrypted files the download info of the
    // (empty) encrypted file name is cleared, so the entry of the file stays
    // until the next sync's deleteStaleDownloadInfos.
    let encrypted_name = ctx.item.borrow().encrypted_file_name.clone();
    ctx.shared
        .journal
        .set_download_info(&encrypted_name, &DownloadInfo::default());
    ctx.shared.journal.commit("download file start2", true);
    // After done() upstream sets the read-only flag; done here before returning.
    {
        let i = ctx.item.borrow();
        let dav_user = ctx.shared.account.dav_user();
        let is_lock_owned_by_current_user = i.lock_owner_id == dav_user;
        let is_user_lock_owned =
            i.lock_owner_type == LockOwnerType::UserLock && is_lock_owned_by_current_user;
        let is_token_lock_owned =
            i.lock_owner_type == LockOwnerType::TokenLock && is_lock_owned_by_current_user;
        if i.locked == LockStatus::LockedItem && !is_user_lock_owned && !is_token_lock_owned {
            filesystem::set_file_read_only(&fname, true);
        } else {
            filesystem::set_file_read_only_weak(
                &fname,
                !i.remote_perm.is_null() && !i.remote_perm.has_permission(Permission::CanWrite),
            );
        }
    }
    let status = if is_conflict {
        Status::Conflict
    } else {
        Status::Success
    };
    done(status, "", ErrorCategory::NoError)
}

#[cfg(test)]
mod tests {
    use super::create_download_tmp_file_name;

    #[test]
    fn derived_download_tmp_file_name() {
        let n = create_download_tmp_file_name("A/b.txt");
        assert!(n.starts_with("A/.b.txt.~"), "{n}");
        let long = "x".repeat(300);
        let n = create_download_tmp_file_name(&long);
        assert!(n.encode_utf16().count() <= 254, "{}", n.len());
        assert!(n.starts_with(".xxx"));
    }
}
