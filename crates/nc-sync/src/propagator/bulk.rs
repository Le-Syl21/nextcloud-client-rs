// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `src/libsync/bulkpropagatorjob.{h,cpp}` (nextcloud/desktop
// v34.0.5), and of the delayed upload logic of `owncloudpropagator.cpp`
// (`isDelayedUploadItem`, `_delayedTasks`, the bulk upload blacklist,
// `PropagateRootDirectory::scheduleDelayedJobs`).

//! Bulk upload: small new or changed files are not uploaded one by one but
//! "delayed" while the job tree runs; once the root's sub jobs are done the
//! root schedules one [`BulkJob`] (`BulkPropagatorJob`) that sends them in
//! one `POST /remote.php/dav/bulk` (see [`super::put_multi_file`]).
//!
//! **Off by default, like upstream v34.0.5**, whose
//! `OwncloudPropagator::isDelayedUploadItem()` returns `false`
//! unconditionally, so that `BulkPropagatorJob` is never scheduled. The
//! experimental opt-in [`crate::SyncOptions::bulk_upload`] restores the
//! condition upstream used before it disabled bulk upload (see
//! [`Propagator::is_delayed_upload_item`]).
//!
//! # Execution model
//!
//! The job lives in the propagator arena like the other jobs. Upstream
//! starts every file's checksum computation from `scheduleSelfOrChild()`
//! and gets the results asynchronously (`ComputeChecksum` runs in a
//! thread); here each result is a posted event, handled in posting order.
//! The `POST` is a future of the propagator loop.
//!
//! Upstream quirks kept on purpose:
//! - `done()` does not update the journal's error blacklist (only the bulk
//!   upload blacklist, an in-memory set of the sync engine, so a failed file
//!   is uploaded alone by the next sync);
//! - a file that failed in the reply gets `done()` twice (once while the
//!   reply is read, once in `finalize()`, with an empty error string), so
//!   `itemCompleted` is emitted twice for it;
//! - the batch size of `handleBatchSize()` is computed but never limits a
//!   batch, and the batch loop sums the sizes in an `int`, which cannot
//!   reach the default 5 GB maximum: every delayed file goes in one batch;
//! - `abort()` is `PropagatorJob`'s default, which leaves the running
//!   `POST` alone; the propagation ends without waiting for it, and its
//!   reply is then dropped (the job is deleted with the propagator).
//! - an item with a rename target (automated rename of leading/trailing
//!   spaces) is removed from the pending set under its new name, so the
//!   batch is never sent; upstream then never finishes the job (here the
//!   propagator's stall detection ends the propagation with an error).

use std::collections::{BTreeMap, HashSet, VecDeque};

use nc_dav::{JobOptions, NetworkError, Reply, Target};
use nc_journal::checksums::{ComputeChecksum, make_checksum_header, upload_checksum_enabled};
use nc_journal::journal::UploadInfo;
use nc_journal::remote_permissions::{Permission, RemotePermissions};
use serde_json::{Map, Value};

use super::put_multi_file::{SingleUploadFileData, put_multi_file};
use super::{Completion, JobId, JobState, NodeKind, PostedEvent, Propagator, PropagatorEvent};
use crate::filesystem;
use crate::item::{ErrorCategory, Instruction, Status, SyncFileItemPtr};

const LOG: &str = "nextcloud.sync.propagator.bulkupload";
const LOG_PROPAGATOR: &str = "nextcloud.sync.propagator";
const LOG_UPLOAD: &str = "nextcloud.sync.propagator.upload";

/// `parallelJobsMaximumCount`.
const PARALLEL_JOBS_MAXIMUM_COUNT: usize = 1;

/// `UploadFileInfo`: the file that is actually sent.
#[derive(Clone, Debug, Default)]
pub(crate) struct UploadFileInfo {
    file: String,
    /// the full path on disk.
    path: String,
    size: i64,
}

/// `BulkUploadItem`.
#[derive(Clone)]
struct BulkUploadItem {
    item: SyncFileItemPtr,
    remote_path: String,
    local_path: String,
    file_size: i64,
    headers: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// `BulkPropagatorJob`.
pub(crate) struct BulkJob {
    items: VecDeque<SyncFileItemPtr>,
    /// network jobs that are currently in transit
    jobs: usize,
    pending_checksum_files: HashSet<String>,
    files_to_upload: Vec<BulkUploadItem>,
    sent_total: i64,
    final_status: Status,
    current_batch_size: i32,
}

impl BulkJob {
    pub(crate) fn new(items: VecDeque<SyncFileItemPtr>) -> Self {
        let current_batch_size = items.len() as i32;
        Self {
            items,
            jobs: 0,
            pending_checksum_files: HashSet::new(),
            files_to_upload: Vec::new(),
            sent_total: 0,
            final_status: Status::NoStatus,
            current_batch_size,
        }
    }
}

/// The checksum steps of a file, delivered as posted events (upstream:
/// `ComputeChecksum::done` from the checksum thread).
pub(crate) enum ChecksumStep {
    /// `slotComputeTransmissionChecksum` started; its result is due.
    Transmission,
    /// `slotComputeMd5Checksum` started with the transmission checksum.
    Md5 {
        transmission_type: Vec<u8>,
        transmission_checksum: Vec<u8>,
    },
}

/// `getEtagFromJsonReply`.
fn get_etag_from_json_reply(reply: &Map<String, Value>) -> Vec<u8> {
    let parse = |key: &str| nc_dav::xml::parse_etag(json_string(reply, key).as_bytes());
    let oc_etag = parse("OC-ETag");
    let etag_upper = parse("ETag");
    let etag = parse("etag");
    let mut ret = oc_etag.clone();
    if ret.is_empty() {
        ret = etag_upper.clone();
    }
    if ret.is_empty() {
        ret = etag.clone();
    }
    if !oc_etag.is_empty() && oc_etag != etag && oc_etag != etag_upper {
        log::debug!(target: LOG, "Quite peculiar, we have an etag != OC-Etag [no problem!] {etag:?} {etag_upper:?} {oc_etag:?}");
    }
    ret
}

/// `QJsonValue::toString()`: empty unless the value is a string.
fn json_string(reply: &Map<String, Value>, key: &str) -> String {
    reply
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// `getHeaderFromJsonReply`.
fn get_header_from_json_reply(reply: &Map<String, Value>, header_name: &str) -> Vec<u8> {
    json_string(reply, header_name).into_bytes()
}

impl Propagator {
    fn bulk(&self, id: JobId) -> &BulkJob {
        match &self.nodes[id].kind {
            NodeKind::Bulk(b) => b,
            _ => panic!("not a bulk job"),
        }
    }

    fn bulk_mut(&mut self, id: JobId) -> &mut BulkJob {
        match &mut self.nodes[id].kind {
            NodeKind::Bulk(b) => b,
            _ => panic!("not a bulk job"),
        }
    }

    // ------------------------------------------------------------------
    // OwncloudPropagator: delayed tasks and the bulk upload blacklist

    /// `isDelayedUploadItem(item)`.
    ///
    /// Upstream v34.0.5 returns `false` unconditionally (bulk upload is
    /// disabled); so does this port unless [`crate::SyncOptions::bulk_upload`]
    /// is set. The condition below is the one upstream had before disabling
    /// it (not in the shallow v34.0.5 clone the port is checked against:
    /// written from upstream's earlier releases, and checked against the bulk
    /// job's own requirements and the six bulk upload tests): the server
    /// announces bulk upload, the delayed jobs are not being scheduled yet,
    /// the file is smaller than the minimum chunk size, not end-to-end
    /// encrypted (nor in an encrypted folder) and not in the bulk upload
    /// blacklist.
    pub(crate) fn is_delayed_upload_item(&self, item: &SyncFileItemPtr) -> bool {
        if !self.shared.options.bulk_upload {
            return false;
        }
        let (file, size, encrypted) = {
            let i = item.borrow();
            (i.file.clone(), i.size, i.is_encrypted())
        };
        let check_file_should_be_encrypted = || -> bool {
            let parent_path = match file.rfind('/') {
                Some(p) => &file[..p],
                None => "",
            };
            let Ok(parent_rec) = self.shared.journal.get_file_record(parent_path.as_bytes()) else {
                return false;
            };
            let parent_encrypted = parent_rec
                .as_ref()
                .is_some_and(|r| r.is_valid() && r.is_e2e_encrypted());
            self.shared
                .account
                .capabilities()
                .client_side_encryption_available()
                && parent_encrypted
        };
        self.shared.account.capabilities().bulk_upload()
            && !self.schedule_delayed_tasks
            && !encrypted
            && self.shared.options.min_chunk_size() > size
            && !self.is_in_bulk_upload_black_list(&file)
            && !check_file_should_be_encrypted()
    }

    /// `addToBulkUploadBlackList`.
    fn add_to_bulk_upload_black_list(&self, file: &str) {
        log::debug!(target: LOG_PROPAGATOR, "black list for bulk upload {file}");
        self.bulk_upload_black_list
            .borrow_mut()
            .insert(file.to_owned());
    }

    /// `removeFromBulkUploadBlackList`.
    fn remove_from_bulk_upload_black_list(&self, file: &str) {
        log::debug!(target: LOG_PROPAGATOR, "black list for bulk upload {file}");
        self.bulk_upload_black_list.borrow_mut().remove(file);
    }

    /// `isInBulkUploadBlackList`.
    fn is_in_bulk_upload_black_list(&self, file: &str) -> bool {
        self.bulk_upload_black_list.borrow().contains(file)
    }

    /// `PropagateRootDirectory::scheduleDelayedJobs`.
    pub(super) fn schedule_delayed_jobs(&mut self, root: JobId) -> bool {
        // setScheduleDelayedTasks(true)
        self.schedule_delayed_tasks = true;
        let items = std::mem::take(&mut self.delayed_tasks);
        let bulk = self.add_node(NodeKind::Bulk(BulkJob::new(items)));
        let sub_jobs = self.directory(root).sub_jobs;
        self.composite_append_job(sub_jobs, bulk);
        self.nodes[sub_jobs].state = JobState::Running;
        self.schedule_self_or_child(sub_jobs)
    }

    // ------------------------------------------------------------------
    // BulkPropagatorJob

    /// `BulkPropagatorJob::scheduleSelfOrChild`.
    pub(super) fn bulk_schedule_self_or_child(&mut self, id: JobId) -> bool {
        {
            let b = self.bulk(id);
            if b.items.is_empty() || !b.pending_checksum_files.is_empty() {
                return false;
            }
        }
        self.nodes[id].state = JobState::Running;
        let max_chunk_size = self.shared.options.max_chunk_size();
        log::debug!(target: LOG, "max chunk size {max_chunk_size}");
        // Upstream sums the sizes in an `int` (`auto batchDataSize = 0`).
        let mut batch_data_size: i32 = 0;
        while i64::from(batch_data_size) <= max_chunk_size {
            let Some(current_item) = self.bulk_mut(id).items.pop_front() else {
                break;
            };
            let (file, size) = {
                let i = current_item.borrow();
                (i.file.clone(), i.size)
            };
            self.bulk_mut(id)
                .pending_checksum_files
                .insert(file.clone());
            batch_data_size = (i64::from(batch_data_size) + size) as i32;
            // QMetaObject::invokeMethod with the default connection: called
            // right away (same thread).
            let file_to_upload = UploadFileInfo {
                path: self.shared.full_local_path(&file),
                file,
                size,
            };
            log::debug!(target: LOG, "Scheduling bulk propagator job: {id} and starting upload of item with file: {} with size: {} with path: {}", file_to_upload.file, file_to_upload.size, file_to_upload.path);
            self.bulk_start_upload_file(id, current_item, file_to_upload);
        }
        let b = self.bulk(id);
        b.items.is_empty() && b.files_to_upload.is_empty()
    }

    /// `handleBatchSize`.
    fn bulk_handle_batch_size(&mut self, id: JobId) -> bool {
        let b = self.bulk_mut(id);
        // no error, no batch size to change
        if b.final_status == Status::Success || b.final_status == Status::NoStatus {
            log::debug!(target: LOG, "No error, no need to change the bulk upload batch size!");
            return true;
        }
        // change batch size before trying it again
        let half_batch_size = (b.items.len() / 2) as i32;
        // we already tried to upload with half of the batch size
        if b.current_batch_size == half_batch_size {
            log::warn!(target: LOG, "There was another error, stop syncing now!");
            return false;
        }
        // try to upload with half of the batch size
        b.current_batch_size = half_batch_size;
        log::warn!(target: LOG, "There was an error, sync again with bulk upload batch size cut to half!");
        true
    }

    /// `startUploadFile`.
    fn bulk_start_upload_file(
        &mut self,
        id: JobId,
        item: SyncFileItemPtr,
        file_to_upload: UploadFileInfo,
    ) {
        if self.shared.abort_requested.get() {
            return;
        }
        // (hasCaseClashAccessibilityProblem only looks for a problem on Windows.)
        // slotComputeTransmissionChecksum: the computation runs in a thread
        // upstream; its result comes back as a posted event.
        self.posted.push_back(PostedEvent::BulkChecksum(
            id,
            item,
            file_to_upload,
            ChecksumStep::Transmission,
        ));
    }

    /// The result of a checksum step (`ComputeChecksum::done`).
    pub(super) fn bulk_checksum_done(
        &mut self,
        id: JobId,
        item: SyncFileItemPtr,
        file_to_upload: UploadFileInfo,
        step: ChecksumStep,
    ) {
        let path = std::path::Path::new(&file_to_upload.path);
        match step {
            ChecksumStep::Transmission => {
                // Compute the transmission checksum.
                let checksum_type = if upload_checksum_enabled() {
                    self.shared
                        .account
                        .capabilities()
                        .preferred_upload_checksum_type()
                } else {
                    Vec::new()
                };
                let mut c = ComputeChecksum::new();
                c.set_checksum_type(&checksum_type);
                let (content_checksum_type, content_checksum) = c.start(path);
                if self
                    .shared
                    .account
                    .bulk_upload_needs_legacy_checksum_header()
                {
                    // slotComputeMd5Checksum
                    self.posted.push_back(PostedEvent::BulkChecksum(
                        id,
                        item,
                        file_to_upload,
                        ChecksumStep::Md5 {
                            transmission_type: content_checksum_type,
                            transmission_checksum: content_checksum,
                        },
                    ));
                } else {
                    self.bulk_slot_start_upload(
                        id,
                        item,
                        file_to_upload,
                        &content_checksum_type,
                        &content_checksum,
                        &[],
                    );
                }
            }
            ChecksumStep::Md5 {
                transmission_type,
                transmission_checksum,
            } => {
                let mut c = ComputeChecksum::new();
                c.set_checksum_type(b"MD5");
                let (_, md5) = c.start(path);
                self.bulk_slot_start_upload(
                    id,
                    item,
                    file_to_upload,
                    &transmission_type,
                    &transmission_checksum,
                    &md5,
                );
            }
        }
    }

    /// `slotStartUpload`.
    fn bulk_slot_start_upload(
        &mut self,
        id: JobId,
        item: SyncFileItemPtr,
        mut file_to_upload: UploadFileInfo,
        transmission_checksum_type: &[u8],
        transmission_checksum: &[u8],
        md5_checksum: &[u8],
    ) {
        let transmission_checksum_header =
            make_checksum_header(transmission_checksum_type, transmission_checksum);
        item.borrow_mut().checksum_header = transmission_checksum_header.clone();
        let file = item.borrow().file.clone();
        let full_file_path = file_to_upload.path.clone();
        let original_file_path = self.shared.full_local_path(&file);
        if !filesystem::file_exists(&full_file_path) {
            self.bulk_mut(id).pending_checksum_files.remove(&file);
            self.bulk_done(
                id,
                &item,
                Status::SoftError,
                format!("File Removed (start upload) {full_file_path}"),
                ErrorCategory::GenericError,
            );
            self.bulk_check_propagation_is_done(id);
            return;
        }
        // the item value was set in PropagateUploadFile::start()
        let prev_modtime = item.borrow().modtime;
        // but a potential checksum calculation could have taken some time during which the file could
        // have been changed again, so better check again here.
        let mut modtime = filesystem::get_mod_time(&original_file_path);
        item.borrow_mut().modtime = modtime;
        log::debug!(target: LOG_UPLOAD, "fullFilePath {full_file_path} originalFilePath {original_file_path} prevModtime {prev_modtime} item->_modtime {modtime}");
        if modtime <= 0 {
            let now = crate::utility::current_secs_since_epoch();
            log::info!(target: LOG_UPLOAD, "File {file} has invalid modification time of {modtime} -- trying to update it to {now}");
            if filesystem::set_mod_time(&original_file_path, now) {
                modtime = now;
                item.borrow_mut().modtime = now;
            } else {
                log::warn!(target: LOG_UPLOAD, "Could not update modification time for {file}");
                self.bulk_mut(id).pending_checksum_files.remove(&file);
                self.bulk_done(
                    id,
                    &item,
                    Status::NormalError,
                    format!(
                        "File {file} has invalid modification time. Do not upload to the server."
                    ),
                    ErrorCategory::GenericError,
                );
                self.bulk_check_propagation_is_done(id);
                return;
            }
        }
        if prev_modtime > 0 && prev_modtime != modtime {
            self.shared.another_sync_needed.set(true);
            self.bulk_mut(id).pending_checksum_files.remove(&file);
            log::debug!(target: LOG, "trigger another sync after checking modified time of item {file} prevModtime {prev_modtime} Curr {modtime}");
            self.bulk_done(
                id,
                &item,
                Status::SoftError,
                "Local file changed during syncing. It will be resumed.".to_owned(),
                ErrorCategory::GenericError,
            );
            self.bulk_check_propagation_is_done(id);
            return;
        }
        file_to_upload.size = filesystem::get_size(&full_file_path);
        item.borrow_mut().size = filesystem::get_size(&original_file_path);
        // But skip the file if the mtime is too close to 'now'!
        // That usually indicates a file that is still being changed
        // or not yet fully copied to the destination.
        if super::upload::file_is_still_changing(
            modtime,
            self.shared.options.minimum_file_age_for_upload,
        ) {
            self.shared.another_sync_needed.set(true);
            self.bulk_mut(id).pending_checksum_files.remove(&file);
            self.bulk_done(
                id,
                &item,
                Status::SoftError,
                "Local file changed during sync.".to_owned(),
                ErrorCategory::GenericError,
            );
            self.bulk_check_propagation_is_done(id);
            return;
        }
        self.bulk_do_start_upload(
            id,
            item,
            file_to_upload,
            transmission_checksum_header,
            md5_checksum.to_vec(),
        );
    }

    /// `headers(item)`: the base headers of a part.
    fn bulk_headers(&self, item: &SyncFileItemPtr) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let i = item.borrow();
        let mut headers = BTreeMap::new();
        headers.insert(
            b"Content-Type".to_vec(),
            b"application/octet-stream".to_vec(),
        );
        headers.insert(b"X-File-Mtime".to_vec(), i.modtime.to_string().into_bytes());
        if std::env::var("OWNCLOUD_LAZYOPS")
            .ok()
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
            != 0
        {
            headers.insert(b"OC-LazyOps".to_vec(), b"true".to_vec());
        }
        if !i.etag.is_empty()
            && i.etag != b"empty_etag"
            && i.instruction != Instruction::New // On new files never send a If-Match
            && i.instruction != Instruction::TypeChange
        {
            // We add quotes because the owncloud server always adds quotes around the etag, and
            //  csync_owncloud.c's owncloud_file_id always strips the quotes.
            let mut v = b"\"".to_vec();
            v.extend_from_slice(&i.etag);
            v.push(b'"');
            headers.insert(b"If-Match".to_vec(), v);
        }
        // Set up a conflict file header pointing to the original file
        let conflict_record = self.shared.journal.conflict_record(i.file.as_bytes());
        if conflict_record.is_valid() {
            headers.insert(b"OC-Conflict".to_vec(), b"1".to_vec());
            if !conflict_record.initial_base_path.is_empty() {
                headers.insert(
                    b"OC-ConflictInitialBasePath".to_vec(),
                    conflict_record.initial_base_path.clone(),
                );
            }
            if !conflict_record.base_file_id.is_empty() {
                headers.insert(
                    b"OC-ConflictBaseFileId".to_vec(),
                    conflict_record.base_file_id.clone(),
                );
            }
            if conflict_record.base_modtime != -1 {
                headers.insert(
                    b"OC-ConflictBaseMtime".to_vec(),
                    conflict_record.base_modtime.to_string().into_bytes(),
                );
            }
            if !conflict_record.base_etag.is_empty() {
                headers.insert(
                    b"OC-ConflictBaseEtag".to_vec(),
                    conflict_record.base_etag.clone(),
                );
            }
        }
        headers
    }

    /// `doStartUpload`.
    fn bulk_do_start_upload(
        &mut self,
        id: JobId,
        item: SyncFileItemPtr,
        mut file_to_upload: UploadFileInfo,
        transmission_checksum_header: Vec<u8>,
        md5_checksum_header: Vec<u8>,
    ) {
        if self.shared.abort_requested.get() {
            return;
        }
        // write the checksum in the database, so if the POST is sent
        // to the server, but the connection drops before we get the etag, we can check the checksum
        // in reconcile (issue #5106)
        {
            let i = item.borrow();
            let pi = UploadInfo {
                valid: true,
                chunk_upload_v1: 0,
                transferid: 0, // We set a null transfer id because it is not chunked.
                modtime: i.modtime,
                error_count: 0,
                content_checksum: i.checksum_header.clone(),
                size: i.size,
            };
            self.shared.journal.set_upload_info(&i.file, &pi);
            self.shared.journal.commit("Upload info", true);
        }
        let mut current_headers = self.bulk_headers(&item);
        current_headers.insert(
            b"Content-Length".to_vec(),
            file_to_upload.size.to_string().into_bytes(),
        );
        let (file, rename_target) = {
            let i = item.borrow();
            (i.file.clone(), i.rename_target.clone())
        };
        if !rename_target.is_empty() && file != rename_target {
            // Try to rename the file
            let original_file_path_absolute = self.shared.full_local_path(&file);
            let new_file_path_absolute = self.shared.full_local_path(&rename_target);
            if std::fs::rename(&original_file_path_absolute, &new_file_path_absolute).is_err() {
                self.bulk_done(
                    id,
                    &item,
                    Status::NormalError,
                    "File contains leading or trailing spaces and couldn't be renamed".to_owned(),
                    ErrorCategory::GenericError,
                );
                return;
            }
            log::warn!(target: LOG, "{file} {rename_target}");
            item.borrow_mut().file = rename_target.clone();
            file_to_upload.file = rename_target.clone();
            file_to_upload.path = self.shared.full_local_path(&file_to_upload.file);
            let modtime = filesystem::get_mod_time(&new_file_path_absolute);
            item.borrow_mut().modtime = modtime;
            if modtime <= 0 {
                let now = crate::utility::current_secs_since_epoch();
                log::info!(target: LOG_UPLOAD, "File {rename_target} has invalid modification time of {modtime} -- trying to update it to {now}");
                if filesystem::set_mod_time(&new_file_path_absolute, now) {
                    item.borrow_mut().modtime = now;
                } else {
                    log::warn!(target: LOG_UPLOAD, "Could not update modification time for {rename_target}");
                    self.bulk_mut(id)
                        .pending_checksum_files
                        .remove(&rename_target);
                    self.bulk_done(
                        id,
                        &item,
                        Status::NormalError,
                        format!(
                            "File {rename_target} has invalid modified time. Do not upload to the server."
                        ),
                        ErrorCategory::GenericError,
                    );
                    self.bulk_check_propagation_is_done(id);
                    return;
                }
            }
        }
        let remote_path = self.shared.full_remote_path(&file_to_upload.file);
        if !md5_checksum_header.is_empty() {
            current_headers.insert(b"X-File-MD5".to_vec(), md5_checksum_header);
        }
        current_headers.insert(b"OC-Checksum".to_vec(), transmission_checksum_header);
        let new_upload_file = BulkUploadItem {
            item: item.clone(),
            remote_path,
            local_path: file_to_upload.path.clone(),
            file_size: file_to_upload.size,
            headers: current_headers,
        };
        // (item->_file is the rename target by now, as upstream.)
        let item_file = item.borrow().file.clone();
        let b = self.bulk_mut(id);
        b.files_to_upload.push(new_upload_file);
        b.pending_checksum_files.remove(&item_file);
        if b.pending_checksum_files.is_empty() {
            self.bulk_trigger_upload(id);
        }
    }

    /// `triggerUpload`.
    fn bulk_trigger_upload(&mut self, id: JobId) {
        let mut upload_parameters_data: Vec<SingleUploadFileData> = Vec::new();
        // Upstream sums the sizes in an `int`.
        let mut timeout: i32 = 0;
        let files = self.bulk(id).files_to_upload.clone();
        for (index, single_file) in files.iter().enumerate() {
            // UploadDevice(localPath, 0, fileSize): opened, then read whole by
            // PutMultiFileJob::start (unthrottled there).
            let data = match std::fs::read(&single_file.local_path) {
                Ok(mut d) => {
                    d.truncate(single_file.file_size.max(0) as usize);
                    d
                }
                Err(e) => {
                    let error = e.to_string();
                    log::warn!(target: LOG, "Could not prepare upload device: {error}");
                    // If the file is currently locked, we want to retry the sync
                    // when it becomes available again.
                    if filesystem::is_file_locked(&single_file.local_path) {
                        self.shared.emit(PropagatorEvent::SeenLockedFile(
                            single_file.local_path.clone(),
                        ));
                    }
                    self.bulk_abort_with_error(id, &single_file.item, Status::NormalError, error);
                    self.emit_job_finished(id, Status::NormalError);
                    return;
                }
            };
            let mut headers = single_file.headers.clone();
            headers.insert(
                b"X-File-Path".to_vec(),
                single_file.remote_path.as_bytes().to_vec(),
            );
            self.bulk_mut(id).files_to_upload[index].headers = headers.clone();
            upload_parameters_data.push(SingleUploadFileData {
                data: data.into(),
                headers,
            });
            timeout = (i64::from(timeout) + single_file.file_size) as i32;
        }
        let target = Target::Account("/remote.php/dav/bulk".to_owned());
        let mut opts = JobOptions::with_cancel(self.shared.soft_abort.child_token());
        super::upload::adjust_last_job_timeout(&mut opts, i64::from(timeout));
        self.bulk_mut(id).jobs += 1;
        let account = self.shared.account.clone();
        self.futures.push(Box::pin(async move {
            let (reply, sent) =
                put_multi_file(&account, &target, &upload_parameters_data, &opts).await;
            (id, Completion::BulkPut(Box::new(reply), sent))
        }));
        if self.parallelism(id) == super::Parallelism::FullParallelism
            && self.bulk(id).jobs < PARALLEL_JOBS_MAXIMUM_COUNT
        {
            self.bulk_schedule_self_or_child(id);
        }
    }

    /// `checkPropagationIsDone`.
    fn bulk_check_propagation_is_done(&mut self, id: JobId) {
        if self.bulk(id).items.is_empty() {
            let b = self.bulk(id);
            if b.jobs > 0 || !b.pending_checksum_files.is_empty() {
                // just wait for the other job to finish.
                return;
            }
        } else if self.bulk_handle_batch_size(id) {
            self.bulk_schedule_self_or_child(id);
            return;
        }
        let final_status = self.bulk(id).final_status;
        log::info!(target: LOG, "final status {final_status:?}");
        self.emit_job_finished(id, final_status);
        self.shared.schedule_next_job();
    }

    /// `slotOnErrorStartFolderUnlock` is `done()` with a log line; `done()`.
    fn bulk_done(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        status: Status,
        error_string: String,
        category: ErrorCategory,
    ) {
        {
            let mut i = item.borrow_mut();
            i.status = status;
            i.error_string = error_string.clone();
            log::info!(target: LOG, "Item completed {} {:?} {:?} {}", i.destination(), i.status, i.instruction, i.error_string);
            // handleFileRestoration
            if i.is_restoration {
                if i.status == Status::Success || i.status == Status::Conflict {
                    i.status = Status::Restoration;
                } else {
                    i.error_string
                        .push_str(&format!("Restoration failed: {error_string}"));
                }
            } else if i.error_string.is_empty() {
                i.error_string = error_string;
            }
            if self.shared.abort_requested.get()
                && matches!(i.status, Status::NormalError | Status::FatalError)
            {
                // an abort request is ongoing. Change the status to Soft-Error
                i.status = Status::SoftError;
            }
        }
        if item.borrow().status != Status::Success {
            // Blacklist handling
            let file = item.borrow().file.clone();
            self.add_to_bulk_upload_black_list(&file);
            self.shared.another_sync_needed.set(true);
        }
        self.bulk_handle_job_done_errors(id, item, status);
        self.emit_item_completed(item, category);
    }

    /// `handleJobDoneErrors`.
    fn bulk_handle_job_done_errors(&mut self, id: JobId, item: &SyncFileItemPtr, status: Status) {
        let item_status = {
            let i = item.borrow();
            if i.has_error_status() {
                log::warn!(target: LOG_PROPAGATOR, "Could not complete propagation of {} by {id} with status {:?} and error: {}", i.destination(), i.status, i.error_string);
            } else {
                log::info!(target: LOG_PROPAGATOR, "Completed propagation of {} by {id} with status {:?}", i.destination(), i.status);
            }
            i.status
        };
        if item_status == Status::FatalError {
            // Abort all remaining jobs.
            self.abort();
        }
        let b = self.bulk_mut(id);
        match item_status {
            Status::Success => {}
            Status::DetailError => {
                b.final_status = Status::DetailError;
                log::info!(target: LOG, "modify final status DetailError {:?} {status:?}", b.final_status);
            }
            _ => {
                b.final_status = Status::NormalError;
                log::info!(target: LOG, "modify final status NormalError {:?} {status:?}", b.final_status);
            }
        }
    }

    /// `abortWithError`: `abort(Synchronous)` is `PropagatorJob`'s default,
    /// which does nothing, then `done()`.
    fn bulk_abort_with_error(
        &mut self,
        id: JobId,
        item: &SyncFileItemPtr,
        status: Status,
        error: String,
    ) {
        self.bulk_done(id, item, status, error, ErrorCategory::GenericError);
    }

    /// `checkResettingErrors`.
    fn bulk_check_resetting_errors(&self, item: &SyncFileItemPtr) {
        let (code, file) = {
            let i = item.borrow();
            (i.http_error_code, i.file.clone())
        };
        if code == 412
            || self
                .shared
                .account
                .capabilities()
                .http_error_codes_that_reset_failing_chunked_uploads()
                .contains(&i32::from(code))
        {
            let mut upload_info = self.shared.journal.get_upload_info(&file);
            upload_info.error_count += 1;
            if upload_info.error_count > 3 {
                log::info!(target: LOG, "Reset transfer of {file} due to repeated error {code}");
                upload_info = UploadInfo::default();
            } else {
                log::info!(target: LOG, "Error count for maybe-reset error {code} on file {file} is {}", upload_info.error_count);
            }
            self.shared.journal.set_upload_info(&file, &upload_info);
            self.shared.journal.commit("Upload info", true);
        }
    }

    /// The `POST` finished (`slotPutFinished`).
    pub(super) fn bulk_put_finished(&mut self, id: JobId, reply: Reply, sent: i64) {
        if self.shared.abort_requested.get() && reply.error == NetworkError::OperationCanceledError
        {
            // Upstream does not abort the job: it is deleted with the
            // propagator, so its result is never handled.
            return;
        }
        // uploadProgress(sent, total), connected once per file (the whole
        // body has been sent once the reply is there).
        if reply.http_status != 0 && sent != 0 {
            for f in self.bulk(id).files_to_upload.clone() {
                // slotUploadProgress
                self.bulk_mut(id).sent_total += sent;
                let total = self.bulk(id).sent_total;
                self.shared.report_progress(&f.item.borrow().clone(), total);
            }
        }
        // slotJobDestroyed: remove it from the _jobs list
        let b = self.bulk_mut(id);
        b.jobs = b.jobs.saturating_sub(1);
        let job_error = reply.error;
        let full_reply_object: Map<String, Value> = match serde_json::from_slice(&reply.body) {
            Ok(Value::Object(o)) => o,
            _ => Map::new(),
        };
        for single_file in self.bulk(id).files_to_upload.clone() {
            let Some(single_reply) = full_reply_object.get(&single_file.remote_path) else {
                if job_error != NetworkError::NoError {
                    single_file.item.borrow_mut().status = Status::NormalError;
                    self.bulk_abort_with_error(
                        id,
                        &single_file.item,
                        Status::NormalError,
                        format!("Network error: {}", job_error as i32),
                    );
                }
                continue;
            };
            let single_reply_object = match single_reply {
                Value::Object(o) => o.clone(),
                _ => Map::new(),
            };
            self.bulk_put_finished_one_file(id, &single_file, &reply, &single_reply_object);
        }
        self.bulk_finalize(id, &full_reply_object);
    }

    /// `slotPutFinishedOneFile`.
    fn bulk_put_finished_one_file(
        &mut self,
        id: JobId,
        single_file: &BulkUploadItem,
        reply: &Reply,
        file_reply: &Map<String, Value>,
    ) {
        let item = &single_file.item;
        log::info!(target: LOG, "{} file headers {file_reply:?}", item.borrow().file);
        let ok = file_reply
            .get("error")
            .is_some_and(|v| !v.as_bool().unwrap_or(false));
        {
            let mut i = item.borrow_mut();
            i.http_error_code = if ok { 200 } else { 412 };
            i.response_time_stamp = reply.response_timestamp();
            i.request_id = reply.request_id.as_bytes().to_vec();
        }
        if !ok {
            // commonErrorHandling
            let message = json_string(file_reply, "message");
            // Ensure errors that should eventually reset the chunked upload are tracked.
            self.bulk_check_resetting_errors(item);
            self.bulk_abort_with_error(id, item, Status::NormalError, message);
            let (name, message) = super::get_exception_from_reply(reply);
            let mut i = item.borrow_mut();
            i.error_exception_name = name;
            i.error_exception_message = message;
            return;
        }
        item.borrow_mut().status = Status::Success;
        // upload succeeded, so remove from black list
        let file = item.borrow().file.clone();
        self.remove_from_bulk_upload_black_list(&file);
        // Check the file again post upload.
        // Two cases must be considered separately: If the upload is finished,
        // the file is on the server and has a changed ETag. In that case,
        // the etag has to be properly updated in the client journal, and because
        // of that we can bail out here with an error. But we can reschedule a
        // sync ASAP.
        // But if the upload is ongoing, because not all chunks were uploaded
        // yet, the upload can be stopped and an error can be displayed, because
        // the server hasn't registered the new file yet.
        let etag = get_etag_from_json_reply(file_reply);
        let finished = !etag.is_empty();
        let full_file_path = self.shared.full_local_path(&file);
        // Check if the file still exists
        if !filesystem::file_exists(&full_file_path) {
            if !finished {
                self.bulk_abort_with_error(
                    id,
                    item,
                    Status::SoftError,
                    "The local file was removed during sync.".to_owned(),
                );
                return;
            } else {
                self.shared.another_sync_needed.set(true);
            }
        }
        // Check whether the file changed since discovery. the file check here is the original and not the temporary.
        let (size, modtime) = {
            let i = item.borrow();
            (i.size, i.modtime)
        };
        if !filesystem::verify_file_unchanged(&full_file_path, size, modtime) {
            self.shared.another_sync_needed.set(true);
            if !finished {
                self.bulk_abort_with_error(
                    id,
                    item,
                    Status::SoftError,
                    "Local file changed during sync.".to_owned(),
                );
                // FIXME:  the legacy code was retrying for a few seconds.
                //         and also checking that after the last chunk, and removed the file in case of INSTRUCTION_NEW
                return;
            }
        }
        let mut i = item.borrow_mut();
        // the file id should only be empty for new files up- or downloaded
        // computeFileId
        let fid = get_header_from_json_reply(file_reply, "OC-FileID");
        if !fid.is_empty() {
            if !i.file_id.is_empty() && i.file_id != fid {
                log::warn!(target: LOG, "File ID changed! {:?} {:?}", String::from_utf8_lossy(&i.file_id), String::from_utf8_lossy(&fid));
            }
            i.file_id = fid;
        }
        let destination = i.destination().to_owned();
        if let Ok(Some(old_record)) = self.shared.journal.get_file_record(destination.as_bytes())
            && old_record.is_valid()
            && old_record.etag != i.etag
        {
            i.update_lock_state_from_db_record(&old_record);
        }
        i.etag = etag;
        i.file_id = get_header_from_json_reply(file_reply, "fileid");
        i.remote_perm =
            RemotePermissions::from_server_string(&json_string(file_reply, "permissions"));
        i.is_shared = i.remote_perm.has_permission(Permission::IsShared) || i.shared_by_me;
        i.last_share_state_fetched_timestamp = jiff::Timestamp::now().as_millisecond();
        if get_header_from_json_reply(file_reply, "X-OC-MTime") != b"accepted" {
            // X-OC-MTime is supported since owncloud 5.0.   But not when chunking.
            // Normally Owncloud 6 always puts X-OC-MTime
            log::warn!(target: LOG, "Server does not support X-OC-MTime {}", json_string(file_reply, "X-OC-MTime"));
            // Well, the mtime was not set
        }
    }

    /// `finalizeOneFile`.
    fn bulk_finalize_one_file(&mut self, id: JobId, one_file: &BulkUploadItem) {
        // Update the database entry
        let result = self.shared.update_metadata(&one_file.item.borrow());
        if let Err(e) = result {
            self.bulk_done(
                id,
                &one_file.item,
                Status::FatalError,
                format!("Error updating metadata: {e}"),
                ErrorCategory::GenericError,
            );
            return;
        }
        // (The online-only pin state reset needs virtual files.)
        // Remove from the progress database:
        let file = one_file.item.borrow().file.clone();
        self.shared
            .journal
            .set_upload_info(&file, &UploadInfo::default());
        self.shared.journal.commit("upload file start", true);
    }

    /// `finalize(fullReply)`.
    fn bulk_finalize(&mut self, id: JobId, full_reply: &Map<String, Value>) {
        log::debug!(target: LOG, "Received a full reply {full_reply:?}");
        let mut index = 0;
        while index < self.bulk(id).files_to_upload.len() {
            let single_file = self.bulk(id).files_to_upload[index].clone();
            if !full_reply.contains_key(&single_file.remote_path) {
                index += 1;
                continue;
            }
            if !single_file.item.borrow().has_error_status() {
                self.bulk_finalize_one_file(id, &single_file);
                single_file.item.borrow_mut().status = Status::Success;
            }
            let status = single_file.item.borrow().status;
            self.bulk_done(
                id,
                &single_file.item,
                status,
                String::new(),
                ErrorCategory::GenericError,
            );
            self.bulk_mut(id).files_to_upload.remove(index);
        }
        self.bulk_check_propagation_is_done(id);
    }
}
