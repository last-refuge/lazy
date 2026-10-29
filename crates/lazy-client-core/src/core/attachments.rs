//! Files on messages: composer chips, uploads, downloads, and image previews (slice 3A, D17).
//!
//! The core never reads a file. A chosen file is copied into the runtime's outbox
//! (`Effect::StageFile`), uploaded from there (`Effect::UploadFile`, three at a time, repeated with
//! the same id after an unknown outcome), and named by its id in the message that sends it. A
//! message with files is journaled with their ids and sizes, so a restart can find the copies and
//! finish the upload. Downloads and previews are requests the runtime answers with a path or
//! bytes the core only hands on.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use lazy_domain::attachment::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_MESSAGE, sanitize_file_name,
};
use lazy_domain::{AttachmentId, ConversationId, MessageDocument, OperationId};
use lazy_protocol::{Attachment, AttachmentResponse, ErrorCode};

use crate::attachments::{
    AttachmentState, ComposerAttachment, ComposerTarget, DownloadState, FileSource, ImageData,
    ImageState, JournalAttachment, LocalFile, MAX_CONCURRENT_IMAGE_FETCHES, MAX_CONCURRENT_UPLOADS,
    MAX_HELD_FULL_IMAGES, MAX_HELD_THUMBNAIL_BYTES, MAX_HELD_THUMBNAILS, MAX_QUEUED_IMAGE_FETCHES,
    MAX_UPLOAD_ATTEMPTS, PendingFile, PendingFileState, StagedFile, TransferError, UploadProgress,
    guess_media_type,
};
use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{Delivery, RequestError, RetryKind, SessionPhase};

use super::{ClientCore, OpStage};

/// Where one file stands, inside the core.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// Being copied into the outbox.
    Reading,
    /// In the outbox, not on its way to the server, not known to be there.
    Stopped,
    /// Waiting for one of the upload slots.
    Queued,
    Uploading {
        sent: u64,
        total: u64,
    },
    /// The server holds it.
    Uploaded,
    Failed {
        reason: String,
        can_retry: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Composer(ComposerTarget),
    Operation(OperationId),
}

#[derive(Debug)]
struct FileEntry {
    conversation: ConversationId,
    name: String,
    size: u64,
    media_type: String,
    /// Where the file came from, kept while it is in a composer so a chip can be previewed and
    /// a cancelled copy started again.
    source: Option<FileSource>,
    /// The copy in the outbox is complete.
    staged: bool,
    owner: Owner,
    stage: Stage,
    /// The upload attempt whose reports count; older ones are ignored.
    generation: u64,
    /// Uploads of unknown outcome so far.
    attempts: u32,
}

/// What the core keeps about files.
#[derive(Debug, Default)]
pub(super) struct Files {
    entries: HashMap<AttachmentId, FileEntry>,
    /// The files of each composer, in the order they were added.
    composer_order: HashMap<ComposerTarget, Vec<AttachmentId>>,
    /// The chips as the UI reads them, rebuilt from the entries whenever one changes.
    composers: HashMap<ComposerTarget, Vec<ComposerAttachment>>,
    /// The files of each message being sent (or failed and waiting for a retry), in order.
    op_files: HashMap<OperationId, Vec<AttachmentId>>,
    /// Files waiting for an upload slot.
    queue: VecDeque<AttachmentId>,
    next_generation: u64,
    /// Threads whose first message was deleted with no replies left.
    ended_threads: HashSet<lazy_domain::MessageId>,
    downloads: HashMap<AttachmentId, DownloadState>,
    download_names: HashMap<AttachmentId, String>,
    thumbnails: HashMap<AttachmentId, ImageState>,
    full_images: HashMap<AttachmentId, ImageState>,
    /// Ready thumbnails, least recently used first.
    thumbnail_order: VecDeque<AttachmentId>,
    thumbnail_bytes: usize,
    /// Ready full images, least recently used first.
    full_order: VecDeque<AttachmentId>,
    image_queue: VecDeque<(AttachmentId, bool)>,
    image_in_flight: HashSet<(AttachmentId, bool)>,
}

impl Files {
    /// Files being uploaded now.
    pub(super) fn uploading(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry.stage, Stage::Uploading { .. }))
            .count()
    }
}

impl ClientCore {
    /// The files attached to a composer, oldest first. Empty when there are none.
    pub fn composer_attachments(&self, target: ComposerTarget) -> &[ComposerAttachment] {
        self.files
            .composers
            .get(&target)
            .map_or(&[], |list| list.as_slice())
    }

    /// Where a download started with `Input::DownloadAttachment` stands.
    pub fn download(&self, attachment: AttachmentId) -> Option<&DownloadState> {
        self.files.downloads.get(&attachment)
    }

    /// The thumbnail of an attachment, once asked for with `Input::LoadThumbnail`.
    pub fn thumbnail(&self, attachment: AttachmentId) -> Option<&ImageState> {
        self.files.thumbnails.get(&attachment)
    }

    /// The whole image of an attachment, once asked for with `Input::LoadFullImage`.
    pub fn full_image(&self, attachment: AttachmentId) -> Option<&ImageState> {
        self.files.full_images.get(&attachment)
    }

    pub(super) fn attachment_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::AttachFiles { target, files } => self.attach_files(target, files, out),
            Input::RemoveAttachment { target, attachment } => {
                self.remove_attachment(target, attachment, out)
            }
            Input::CancelAttachment { target, attachment } => {
                self.cancel_attachment(target, attachment, out)
            }
            Input::RetryAttachment { target, attachment } => {
                self.retry_attachment(target, attachment, out)
            }
            Input::DiscardOperation {
                conversation,
                operation,
            } => self.discard_operation(conversation, operation, out),
            Input::DownloadAttachment {
                attachment,
                destination,
            } => self.download_attachment(attachment, destination, out),
            Input::CancelDownload(attachment) => self.cancel_download(attachment, out),
            Input::DismissDownload(attachment) => self.dismiss_download(attachment, out),
            Input::LoadThumbnail(attachment) => self.request_image(attachment, false, out),
            Input::LoadFullImage(attachment) => self.request_image(attachment, true, out),
            Input::ReleaseFullImage(attachment) => self.release_full_image(attachment),
            Input::FileStaged {
                attachment,
                generation,
                result,
            } => self.file_staged(attachment, generation, result, out),
            Input::UploadProgress {
                attachment,
                generation,
                sent,
                total,
            } => self.upload_progress(attachment, generation, sent, total, out),
            Input::UploadResult {
                attachment,
                generation,
                result,
            } => self.upload_result(attachment, generation, result, out),
            Input::DownloadProgress {
                attachment,
                received,
                total,
            } => self.download_progress(attachment, received, total, out),
            Input::DownloadResult { attachment, result } => {
                self.download_result(attachment, result, out)
            }
            Input::ImageLoaded {
                attachment,
                full,
                result,
            } => self.image_loaded(attachment, full, result, out),
            _ => {}
        }
    }
}

// ----- Composer files -----

impl ClientCore {
    fn target_conversation(&self, target: ComposerTarget) -> Option<ConversationId> {
        match target {
            ComposerTarget::Conversation(conversation) => {
                self.conversation(conversation).map(|_| conversation)
            }
            ComposerTarget::Thread(root) => self.thread_conversation(root),
        }
    }

    fn attach_files(&mut self, target: ComposerTarget, files: Vec<LocalFile>, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || files.is_empty() {
            return;
        }
        let Some(conversation) = self.target_conversation(target) else {
            return;
        };
        if self
            .conversation(conversation)
            .is_some_and(|conversation| conversation.archived)
        {
            let notice = "This conversation is archived, so files can't be attached.";
            self.set_notice(Some(notice.to_owned()), out);
            return;
        }
        let held = self.files.composer_order.get(&target).map_or(0, Vec::len);
        let room = MAX_ATTACHMENTS_PER_MESSAGE.saturating_sub(held);
        let dropped = files.len().saturating_sub(room);
        for file in files.into_iter().take(room) {
            self.add_composer_file(target, conversation, file, out);
        }
        if dropped > 0 {
            let notice = format!("A message can have at most {MAX_ATTACHMENTS_PER_MESSAGE} files.");
            self.set_notice(Some(notice), out);
        }
        self.refresh_composer(target, out);
    }

    fn add_composer_file(
        &mut self,
        target: ComposerTarget,
        conversation: ConversationId,
        file: LocalFile,
        out: &mut Output,
    ) {
        let name = sanitize_file_name(&file.name);
        let id = AttachmentId::generate();
        let (size, too_large) = match &file.source {
            FileSource::Bytes(bytes) => {
                let size = bytes.len() as u64;
                (size, size > MAX_ATTACHMENT_BYTES)
            }
            FileSource::Path(_) => (0, false),
        };
        self.files.next_generation += 1;
        let mut entry = FileEntry {
            conversation,
            media_type: guess_media_type(&name).to_owned(),
            name,
            size,
            source: Some(file.source.clone()),
            staged: false,
            owner: Owner::Composer(target),
            stage: Stage::Reading,
            generation: self.files.next_generation,
            attempts: 0,
        };
        if too_large {
            entry.stage = Stage::Failed {
                reason: too_large_text(&entry.name, size),
                can_retry: false,
            };
            entry.source = None;
        } else {
            out.effects.push(Effect::StageFile {
                attachment: id,
                generation: entry.generation,
                source: file.source,
            });
        }
        self.files.entries.insert(id, entry);
        self.files
            .composer_order
            .entry(target)
            .or_default()
            .push(id);
    }

    fn remove_attachment(
        &mut self,
        target: ComposerTarget,
        attachment: AttachmentId,
        out: &mut Output,
    ) {
        if self.owner_of(attachment) != Some(Owner::Composer(target)) {
            return;
        }
        if let Some(order) = self.files.composer_order.get_mut(&target) {
            order.retain(|id| *id != attachment);
        }
        self.release_files(&[attachment], out);
        self.refresh_composer(target, out);
        self.pump_uploads(out);
    }

    fn cancel_attachment(
        &mut self,
        target: ComposerTarget,
        attachment: AttachmentId,
        out: &mut Output,
    ) {
        if self.owner_of(attachment) != Some(Owner::Composer(target)) {
            return;
        }
        let Some(entry) = self.files.entries.get_mut(&attachment) else {
            return;
        };
        let cancelled = || Stage::Failed {
            reason: "Cancelled".to_owned(),
            can_retry: true,
        };
        match entry.stage {
            Stage::Reading => {
                entry.stage = Stage::Failed {
                    reason: "Cancelled".to_owned(),
                    can_retry: entry.source.is_some(),
                };
                entry.staged = false;
                out.effects.push(Effect::ReleaseFiles {
                    attachments: vec![attachment],
                });
            }
            Stage::Uploading { .. } => {
                self.files.next_generation += 1;
                entry.generation = self.files.next_generation;
                entry.stage = cancelled();
                out.effects.push(Effect::CancelUpload { attachment });
            }
            Stage::Queued | Stage::Stopped => {
                entry.stage = cancelled();
                self.files.queue.retain(|id| *id != attachment);
            }
            Stage::Uploaded | Stage::Failed { .. } => return,
        }
        self.refresh_composer(target, out);
        self.pump_uploads(out);
    }

    fn retry_attachment(
        &mut self,
        target: ComposerTarget,
        attachment: AttachmentId,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready)
            || self.owner_of(attachment) != Some(Owner::Composer(target))
        {
            return;
        }
        let Some(entry) = self.files.entries.get_mut(&attachment) else {
            return;
        };
        if !matches!(
            entry.stage,
            Stage::Failed {
                can_retry: true,
                ..
            }
        ) {
            return;
        }
        entry.attempts = 0;
        if entry.staged {
            self.queue_upload(attachment, out);
        } else if let Some(source) = entry.source.clone() {
            self.files.next_generation += 1;
            entry.generation = self.files.next_generation;
            entry.stage = Stage::Reading;
            out.effects.push(Effect::StageFile {
                attachment,
                generation: entry.generation,
                source,
            });
        }
        self.refresh_composer(target, out);
    }

    fn owner_of(&self, attachment: AttachmentId) -> Option<Owner> {
        self.files.entries.get(&attachment).map(|entry| entry.owner)
    }

    /// Whether a composer holds any file.
    fn composer_has_files(&self, target: ComposerTarget) -> bool {
        self.files
            .composer_order
            .get(&target)
            .is_some_and(|order| !order.is_empty())
    }

    /// Takes the files of a composer for a message that is being sent. `None` (with a notice)
    /// when one has failed, so a message never goes out silently without a file the person
    /// attached.
    pub(super) fn take_composer_files(
        &mut self,
        target: ComposerTarget,
        out: &mut Output,
    ) -> Option<Vec<AttachmentId>> {
        let ids = self
            .files
            .composer_order
            .get(&target)
            .cloned()
            .unwrap_or_default();
        let failed = ids.iter().find_map(|id| {
            let entry = self.files.entries.get(id)?;
            matches!(entry.stage, Stage::Failed { .. }).then(|| entry.name.clone())
        });
        if let Some(name) = failed {
            let notice = format!("Remove or retry {name} first.");
            self.set_notice(Some(notice), out);
            return None;
        }
        self.files.composer_order.remove(&target);
        if self.files.composers.remove(&target).is_some() {
            out.changes
                .push(Change::ComposerAttachmentsChanged { target });
        }
        Some(ids)
    }

    /// Whether the composer has files, or the text may be empty.
    pub(super) fn composer_allows_empty(&self, target: ComposerTarget) -> bool {
        self.composer_has_files(target)
    }

    /// Releases the files of a composer that is going away (the conversation or thread is gone).
    pub(super) fn forget_composer_files(&mut self, target: ComposerTarget, out: &mut Output) {
        let ids = self
            .files
            .composer_order
            .remove(&target)
            .unwrap_or_default();
        if !ids.is_empty() {
            self.release_files(&ids, out);
            self.pump_uploads(out);
        }
        if self.files.composers.remove(&target).is_some() {
            out.changes
                .push(Change::ComposerAttachmentsChanged { target });
        }
    }

    /// A thread is gone (its first message was deleted with no replies left): the files in its
    /// reply box and those of replies that failed there are released.
    pub(super) fn release_files_of_thread(
        &mut self,
        root: lazy_domain::MessageId,
        out: &mut Output,
    ) {
        self.files.ended_threads.insert(root);
        self.forget_composer_files(ComposerTarget::Thread(root), out);
        let failed: Vec<OperationId> = self
            .parked_replies
            .get(&root)
            .into_iter()
            .flat_map(|replies| replies.items())
            .filter(|row| {
                matches!(
                    row.delivery,
                    Delivery::Failed {
                        retry: RetryKind::NewOperation,
                        ..
                    }
                )
            })
            .filter_map(|row| row.operation_id())
            .collect();
        for operation in failed {
            self.release_op_files(operation, out);
        }
    }

    /// Whether the thread is known to be gone.
    pub(super) fn thread_has_ended(&self, root: lazy_domain::MessageId) -> bool {
        self.files.ended_threads.contains(&root)
    }

    /// Releases every composer file of a conversation and of its threads.
    pub(super) fn forget_conversation_files(
        &mut self,
        conversation: ConversationId,
        out: &mut Output,
    ) {
        let targets: Vec<ComposerTarget> = self
            .files
            .composer_order
            .keys()
            .copied()
            .filter(|target| match target {
                ComposerTarget::Conversation(c) => *c == conversation,
                ComposerTarget::Thread(root) => {
                    self.files
                        .composer_order
                        .get(target)
                        .and_then(|ids| ids.first())
                        .and_then(|id| self.files.entries.get(id))
                        .is_some_and(|entry| entry.conversation == conversation)
                        || self.thread_conversation(*root) == Some(conversation)
                }
            })
            .collect();
        for target in targets {
            self.forget_composer_files(target, out);
        }
    }

    /// Rebuilds the chips of a composer from the entries and announces a change.
    fn refresh_composer(&mut self, target: ComposerTarget, out: &mut Output) {
        let view: Vec<ComposerAttachment> = self
            .files
            .composer_order
            .get(&target)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| {
                        let entry = self.files.entries.get(id)?;
                        Some(ComposerAttachment {
                            id: *id,
                            name: entry.name.clone(),
                            size: entry.size,
                            media_type_guess: entry.media_type.clone(),
                            state: public_state(entry),
                            source: entry.source.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let before = self.files.composers.get(&target).map(Vec::as_slice);
        if before.unwrap_or(&[]) == view.as_slice() {
            return;
        }
        if view.is_empty() {
            self.files.composers.remove(&target);
        } else {
            self.files.composers.insert(target, view);
        }
        out.changes
            .push(Change::ComposerAttachmentsChanged { target });
    }

    /// Redraws whatever shows this file: its chip, or the row of the message it belongs to.
    fn touch_file(&mut self, id: AttachmentId, out: &mut Output) {
        match self.owner_of(id) {
            Some(Owner::Composer(target)) => self.refresh_composer(target, out),
            Some(Owner::Operation(operation)) => self.refresh_op_row(operation, out),
            None => {}
        }
    }

    /// Stops a file's transfers and deletes its copy, and forgets it.
    fn release_files(&mut self, ids: &[AttachmentId], out: &mut Output) {
        let mut released = Vec::new();
        for id in ids {
            let Some(entry) = self.files.entries.remove(id) else {
                continue;
            };
            if matches!(entry.stage, Stage::Uploading { .. }) {
                out.effects.push(Effect::CancelUpload { attachment: *id });
            }
            released.push(*id);
        }
        if !released.is_empty() {
            self.files.queue.retain(|id| !released.contains(id));
            out.effects.push(Effect::ReleaseFiles {
                attachments: released,
            });
        }
    }
}

fn public_state(entry: &FileEntry) -> AttachmentState {
    match &entry.stage {
        Stage::Reading => AttachmentState::Reading,
        Stage::Stopped | Stage::Queued => AttachmentState::Uploading {
            sent: 0,
            total: entry.size,
        },
        Stage::Uploading { sent, total } => AttachmentState::Uploading {
            sent: *sent,
            total: *total,
        },
        Stage::Uploaded => AttachmentState::Uploaded,
        Stage::Failed { reason, can_retry } => AttachmentState::Failed {
            reason: reason.clone(),
            can_retry: *can_retry,
        },
    }
}

fn pending_state(entry: &FileEntry) -> PendingFileState {
    match &entry.stage {
        Stage::Reading => PendingFileState::Reading,
        Stage::Uploaded => PendingFileState::Uploaded,
        Stage::Uploading { sent, total } => PendingFileState::Uploading {
            sent: *sent,
            total: *total,
        },
        Stage::Stopped | Stage::Queued | Stage::Failed { .. } => PendingFileState::Uploading {
            sent: 0,
            total: entry.size,
        },
    }
}

// ----- Staging and uploading -----

impl ClientCore {
    fn file_staged(
        &mut self,
        attachment: AttachmentId,
        generation: u64,
        result: Result<StagedFile, TransferError>,
        out: &mut Output,
    ) {
        let Some(entry) = self.files.entries.get(&attachment) else {
            return;
        };
        if entry.stage != Stage::Reading || entry.generation != generation {
            // Cancelled, removed, or started again meanwhile.
            return;
        }
        match result {
            Ok(staged) if staged.size > MAX_ATTACHMENT_BYTES => {
                let error = TransferError::TooLarge { size: staged.size };
                self.fail_file(attachment, &error, out);
            }
            Ok(staged) => {
                let live = match entry.owner {
                    Owner::Composer(_) => true,
                    Owner::Operation(operation) => self
                        .ops
                        .get(&operation)
                        .is_some_and(|op| op.stage != OpStage::Failed),
                };
                if let Some(entry) = self.files.entries.get_mut(&attachment) {
                    entry.size = staged.size;
                    entry.staged = true;
                    entry.stage = Stage::Stopped;
                    if matches!(entry.owner, Owner::Operation(_)) {
                        entry.source = None;
                    }
                }
                if live {
                    self.queue_upload(attachment, out);
                }
                self.touch_file(attachment, out);
                if let Some(Owner::Operation(operation)) = self.owner_of(attachment) {
                    self.op_staging_progress(operation, out);
                }
            }
            Err(error) => self.fail_file(attachment, &error, out),
        }
    }

    /// Puts a staged file in the line for an upload slot.
    fn queue_upload(&mut self, attachment: AttachmentId, out: &mut Output) {
        let Some(entry) = self.files.entries.get_mut(&attachment) else {
            return;
        };
        entry.stage = Stage::Queued;
        if !self.files.queue.contains(&attachment) {
            self.files.queue.push_back(attachment);
        }
        self.pump_uploads(out);
    }

    /// Starts queued uploads while fewer than [`MAX_CONCURRENT_UPLOADS`] are running.
    pub(super) fn pump_uploads(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let mut started = Vec::new();
        loop {
            let active = self
                .files
                .entries
                .values()
                .filter(|entry| matches!(entry.stage, Stage::Uploading { .. }))
                .count();
            if active >= MAX_CONCURRENT_UPLOADS {
                break;
            }
            let Some(id) = self.files.queue.pop_front() else {
                break;
            };
            let Some(entry) = self.files.entries.get_mut(&id) else {
                continue;
            };
            if entry.stage != Stage::Queued {
                continue;
            }
            self.files.next_generation += 1;
            entry.generation = self.files.next_generation;
            entry.stage = Stage::Uploading {
                sent: 0,
                total: entry.size,
            };
            out.effects.push(Effect::UploadFile {
                attachment: id,
                conversation: entry.conversation,
                generation: entry.generation,
                name: entry.name.clone(),
                media_type: entry.media_type.clone(),
                size: entry.size,
            });
            started.push(id);
        }
        for id in started {
            self.touch_file(id, out);
        }
    }

    fn upload_progress(
        &mut self,
        attachment: AttachmentId,
        generation: u64,
        sent: u64,
        total: u64,
        out: &mut Output,
    ) {
        let Some(entry) = self.files.entries.get_mut(&attachment) else {
            return;
        };
        if entry.generation != generation || !matches!(entry.stage, Stage::Uploading { .. }) {
            return;
        }
        let total = total.max(entry.size);
        let sent = sent.min(total);
        // Progress only moves forward.
        if let Stage::Uploading { sent: before, .. } = entry.stage
            && sent <= before
        {
            return;
        }
        entry.stage = Stage::Uploading { sent, total };
        self.touch_file(attachment, out);
    }

    fn upload_result(
        &mut self,
        attachment: AttachmentId,
        generation: u64,
        result: Result<AttachmentResponse, TransferError>,
        out: &mut Output,
    ) {
        let Some(entry) = self.files.entries.get_mut(&attachment) else {
            return;
        };
        if entry.generation != generation || !matches!(entry.stage, Stage::Uploading { .. }) {
            return;
        }
        match result {
            Ok(_) => {
                entry.stage = Stage::Uploaded;
                entry.attempts = 0;
                self.touch_file(attachment, out);
                self.pump_uploads(out);
                if let Some(Owner::Operation(operation)) = self.owner_of(attachment) {
                    self.op_upload_progress(operation, out);
                }
            }
            Err(TransferError::Request(error)) if error.is_unauthenticated() => {
                entry.stage = Stage::Stopped;
                self.session_expired(out);
            }
            Err(TransferError::Request(RequestError::Unknown(_))) => {
                entry.attempts += 1;
                if entry.attempts < MAX_UPLOAD_ATTEMPTS {
                    // The same upload again: the server treats a repeated id as the same file.
                    self.queue_upload(attachment, out);
                } else {
                    let error = TransferError::Request(RequestError::Unknown(String::new()));
                    self.fail_file(attachment, &error, out);
                }
            }
            Err(error) => self.fail_file(attachment, &error, out),
        }
    }

    /// A file could not be copied or uploaded. A chip shows why; a message that is being sent
    /// fails, and its other files stop.
    fn fail_file(&mut self, attachment: AttachmentId, error: &TransferError, out: &mut Output) {
        self.note_error("upload", error.code());
        let Some(entry) = self.files.entries.get_mut(&attachment) else {
            return;
        };
        let (reason, can_retry) = transfer_failure(&entry.name, error);
        self.files.queue.retain(|id| *id != attachment);
        match entry.owner {
            Owner::Composer(target) => {
                // A file that could not be read has nothing to retry.
                entry.stage = Stage::Failed { reason, can_retry };
                if !entry.staged && !matches!(error, TransferError::Cancelled) {
                    entry.source = entry.source.take().filter(|_| can_retry);
                }
                self.refresh_composer(target, out);
                self.pump_uploads(out);
            }
            Owner::Operation(operation) => {
                entry.stage = Stage::Stopped;
                let reason = if matches!(error, TransferError::Missing) {
                    format!(
                        "A file for this message is no longer available on this computer: {}.",
                        entry.name
                    )
                } else {
                    reason
                };
                self.fail_operation(operation, reason, RetryKind::NewOperation, true, out);
            }
        }
    }
}

fn size_text(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.0} KiB", (bytes_f / KIB).ceil())
    } else {
        format!("{bytes} bytes")
    }
}

fn limit_text() -> String {
    crate::attachments::size_limit_text()
}

fn too_large_text(name: &str, size: u64) -> String {
    format!(
        "{name} is {}; files can be at most {}.",
        size_text(size),
        limit_text()
    )
}

/// Words for a person about why a file did not upload or could not be copied, and whether
/// trying again can help.
fn transfer_failure(name: &str, error: &TransferError) -> (String, bool) {
    match error {
        TransferError::Request(RequestError::Rejected(api)) => match api.code {
            ErrorCode::UnsupportedMedia => (
                format!(
                    "{name} isn't a file Lazy can accept (images must be PNG, JPEG, GIF or WebP \
                     that can be decoded)."
                ),
                false,
            ),
            ErrorCode::PayloadTooLarge => {
                (format!("{name} is larger than {}.", limit_text()), false)
            }
            ErrorCode::LimitReached => (
                "You have too many uploads waiting. Send or remove some files.".to_owned(),
                true,
            ),
            ErrorCode::Forbidden => (
                "You can't attach files in this conversation.".to_owned(),
                false,
            ),
            ErrorCode::NotFound => (
                "That conversation no longer exists, or you can't see it.".to_owned(),
                false,
            ),
            ErrorCode::Conflict => (
                format!("{name} clashes with an earlier upload. Remove it and attach it again."),
                false,
            ),
            ErrorCode::RateLimited => (
                "Too many uploads at once. Wait a moment and try again.".to_owned(),
                true,
            ),
            ErrorCode::Unauthenticated => {
                ("Your session has expired. Sign in again.".to_owned(), true)
            }
            ErrorCode::InvalidRequest
            | ErrorCode::InvalidCredentials
            | ErrorCode::StaleRevision
            | ErrorCode::UsernameTaken
            | ErrorCode::InvitationInvalid
            | ErrorCode::NameTaken => (format!("{name} was refused."), false),
            ErrorCode::Internal | ErrorCode::Unavailable => (
                format!("Couldn't upload {name}. The server had a problem; try again."),
                true,
            ),
        },
        TransferError::Request(RequestError::Unknown(_)) => (
            format!("Couldn't upload {name}. Check your connection."),
            true,
        ),
        TransferError::TooLarge { size } => (too_large_text(name, *size), false),
        TransferError::Unreadable(why) => (
            format!("{name} couldn't be read: {}.", why.trim_end_matches('.')),
            false,
        ),
        TransferError::Missing => (
            format!("{name} is no longer available on this computer."),
            false,
        ),
        TransferError::Disk(why) => (
            format!(
                "Couldn't keep a copy of {name}: {}.",
                why.trim_end_matches('.')
            ),
            false,
        ),
        TransferError::ChecksumMismatch => (
            format!("{name} changed while it was being copied. Remove it and attach it again."),
            false,
        ),
        TransferError::Cancelled => ("Cancelled".to_owned(), true),
    }
}

// ----- Messages with files -----

impl ClientCore {
    /// Gives the files to a message being sent.
    pub(super) fn adopt_files(&mut self, operation: OperationId, ids: &[AttachmentId]) {
        for id in ids {
            if let Some(entry) = self.files.entries.get_mut(id) {
                entry.owner = Owner::Operation(operation);
                // A chip needs its source to be previewed; a message being sent needs it only
                // while the copy is not made (a copy that failed is made again on a retry).
                if entry.staged {
                    entry.source = None;
                }
            }
        }
        if !ids.is_empty() {
            self.files.op_files.insert(operation, ids.to_vec());
        }
    }

    /// A message is sent again and one of its files was never copied (the copy failed): copy it
    /// again from where it came from.
    pub(super) fn restage_files(&mut self, ids: &[AttachmentId], out: &mut Output) {
        for id in ids {
            let Some(entry) = self.files.entries.get_mut(id) else {
                continue;
            };
            if entry.staged || entry.stage != Stage::Stopped {
                continue;
            }
            let Some(source) = entry.source.clone() else {
                continue;
            };
            self.files.next_generation += 1;
            entry.generation = self.files.next_generation;
            entry.stage = Stage::Reading;
            out.effects.push(Effect::StageFile {
                attachment: *id,
                generation: entry.generation,
                source,
            });
        }
    }

    /// The files of a message being sent, in order.
    pub(super) fn op_file_ids(&self, operation: OperationId) -> Vec<AttachmentId> {
        self.files
            .op_files
            .get(&operation)
            .cloned()
            .unwrap_or_default()
    }

    fn op_entries(&self, operation: OperationId) -> impl Iterator<Item = &FileEntry> {
        self.files
            .op_files
            .get(&operation)
            .into_iter()
            .flatten()
            .filter_map(|id| self.files.entries.get(id))
    }

    /// Whether any file of the message is still being copied.
    pub(super) fn op_is_staging(&self, operation: OperationId) -> bool {
        self.op_entries(operation)
            .any(|entry| entry.stage == Stage::Reading)
    }

    /// Whether the server holds every file of the message.
    pub(super) fn op_uploaded(&self, operation: OperationId) -> bool {
        self.op_entries(operation)
            .all(|entry| entry.stage == Stage::Uploaded)
    }

    /// The files as the journal records them.
    pub(super) fn journal_files(&self, operation: OperationId) -> Vec<JournalAttachment> {
        self.files
            .op_files
            .get(&operation)
            .into_iter()
            .flatten()
            .filter_map(|id| {
                let entry = self.files.entries.get(id)?;
                Some(JournalAttachment {
                    id: *id,
                    name: entry.name.clone(),
                    size: entry.size,
                })
            })
            .collect()
    }

    /// Files a restart found in the journal: in the outbox, as far as anyone knows, and not
    /// known to be on the server.
    pub(super) fn restore_op_files(
        &mut self,
        operation: OperationId,
        conversation: ConversationId,
        files: &[JournalAttachment],
    ) {
        for file in files {
            self.files.entries.insert(
                file.id,
                FileEntry {
                    conversation,
                    media_type: guess_media_type(&file.name).to_owned(),
                    name: file.name.clone(),
                    size: file.size,
                    source: None,
                    staged: true,
                    owner: Owner::Operation(operation),
                    stage: Stage::Stopped,
                    generation: 0,
                    attempts: 0,
                },
            );
        }
        if !files.is_empty() {
            self.files
                .op_files
                .insert(operation, files.iter().map(|file| file.id).collect());
        }
    }

    /// What a row shows for the files of a message being sent.
    pub(super) fn op_row_files(
        &self,
        operation: OperationId,
    ) -> (Vec<Attachment>, Option<UploadProgress>) {
        let mut attachments = Vec::new();
        let mut files = Vec::new();
        for id in self.files.op_files.get(&operation).into_iter().flatten() {
            let Some(entry) = self.files.entries.get(id) else {
                continue;
            };
            attachments.push(Attachment {
                id: *id,
                name: entry.name.clone(),
                media_type: entry.media_type.clone(),
                size: entry.size,
                sha256: String::new(),
                width: None,
                height: None,
                has_thumbnail: false,
            });
            files.push(PendingFile {
                id: *id,
                state: pending_state(entry),
            });
        }
        let progress = (!files.is_empty()).then_some(UploadProgress { files });
        (attachments, progress)
    }

    /// Shows the current state of a message's files on its row.
    pub(super) fn refresh_op_row(&mut self, operation: OperationId, out: &mut Output) {
        let Some(op) = self.ops.get(&operation) else {
            return;
        };
        let (conversation, thread) = (op.conversation, op.thread);
        let (attachments, progress) = self.op_row_files(operation);
        self.in_local_timeline(conversation, thread, out, |timeline, changes| {
            timeline.set_local_files(
                conversation,
                crate::model::ItemKey::Operation(operation),
                attachments,
                progress,
                changes,
            );
        });
    }

    /// A file of a message that is waiting for its copies finished; when the last is in, the
    /// message can be journaled.
    fn op_staging_progress(&mut self, operation: OperationId, out: &mut Output) {
        if self
            .ops
            .get(&operation)
            .is_some_and(|op| op.stage == OpStage::Staging)
            && !self.op_is_staging(operation)
        {
            self.persist_send(operation, out);
        }
    }

    /// A file of a message was uploaded; when the last is in, the message can be sent.
    fn op_upload_progress(&mut self, operation: OperationId, out: &mut Output) {
        let uploading = self
            .ops
            .get(&operation)
            .is_some_and(|op| op.stage == OpStage::Uploading);
        if !(uploading && self.op_uploaded(operation)) {
            return;
        }
        if let Some(op) = self.ops.get_mut(&operation) {
            op.stage = OpStage::Queued;
            let conversation = op.conversation;
            self.pump(conversation, out);
        }
    }

    /// The journal entry is durable: upload what is not on the server yet, then send.
    pub(super) fn op_persisted(&mut self, operation: OperationId, out: &mut Output) {
        let Some(op) = self.ops.get_mut(&operation) else {
            return;
        };
        let conversation = op.conversation;
        if self.op_uploaded(operation) {
            if let Some(op) = self.ops.get_mut(&operation) {
                op.stage = OpStage::Queued;
            }
            self.pump(conversation, out);
        } else {
            if let Some(op) = self.ops.get_mut(&operation) {
                op.stage = OpStage::Uploading;
            }
            self.begin_op_uploads(operation, out);
        }
    }

    /// Queues every file of the message that is not on the server or already on its way.
    pub(super) fn begin_op_uploads(&mut self, operation: OperationId, out: &mut Output) {
        let stopped: Vec<AttachmentId> = self
            .files
            .op_files
            .get(&operation)
            .into_iter()
            .flatten()
            .copied()
            .filter(|id| {
                self.files
                    .entries
                    .get(id)
                    .is_some_and(|entry| entry.stage == Stage::Stopped)
            })
            .collect();
        for id in stopped {
            self.queue_upload(id, out);
        }
        self.refresh_op_row(operation, out);
    }

    /// The message is not going out now (it failed, or the session ended): its uploads stop and
    /// its files wait in the outbox, ready for a retry.
    pub(super) fn stop_op_files(&mut self, operation: OperationId, out: &mut Output) {
        let ids = self.op_file_ids(operation);
        if ids.is_empty() {
            return;
        }
        for id in ids {
            let Some(entry) = self.files.entries.get_mut(&id) else {
                continue;
            };
            match entry.stage {
                Stage::Uploading { .. } => {
                    self.files.next_generation += 1;
                    entry.generation = self.files.next_generation;
                    entry.stage = Stage::Stopped;
                    out.effects.push(Effect::CancelUpload { attachment: id });
                }
                Stage::Queued => {
                    entry.stage = Stage::Stopped;
                    self.files.queue.retain(|queued| *queued != id);
                }
                _ => {}
            }
        }
        self.pump_uploads(out);
    }

    /// The message is final (sent, or abandoned): delete its copies.
    pub(super) fn release_op_files(&mut self, operation: OperationId, out: &mut Output) {
        let Some(ids) = self.files.op_files.remove(&operation) else {
            return;
        };
        self.release_files(&ids, out);
        self.pump_uploads(out);
    }

    /// A failed message is being sent again as a new operation: its files go with it, and since
    /// the server may have lost them, they are uploaded again (the same ids, so a copy the server
    /// still holds is simply kept).
    pub(super) fn take_failed_files(&mut self, operation: OperationId) -> Vec<AttachmentId> {
        let ids = self.files.op_files.remove(&operation).unwrap_or_default();
        for id in &ids {
            if let Some(entry) = self.files.entries.get_mut(id)
                && matches!(
                    entry.stage,
                    Stage::Uploaded | Stage::Queued | Stage::Uploading { .. }
                )
            {
                entry.stage = Stage::Stopped;
                entry.attempts = 0;
            }
        }
        self.files.queue.retain(|queued| !ids.contains(queued));
        ids
    }

    fn discard_operation(
        &mut self,
        conversation: ConversationId,
        operation: OperationId,
        out: &mut Output,
    ) {
        let Some((item, thread)) = self.locate_local_row(conversation, operation) else {
            return;
        };
        if !matches!(item.delivery, Delivery::Failed { .. }) {
            return;
        }
        self.in_local_timeline(conversation, thread, out, |timeline, changes| {
            timeline.remove(
                conversation,
                crate::model::ItemKey::Operation(operation),
                changes,
            );
        });
        self.tidy_parked(thread);
        self.release_op_files(operation, out);
        // A failed message whose outcome was never settled still holds its journal entry.
        self.settle_operation(operation, out);
    }

    /// The message is final: every one of its files is released. Used for a message that was
    /// confirmed, and for one that can never be delivered.
    pub(super) fn files_of_message_settled(&mut self, operation: OperationId, out: &mut Output) {
        self.release_op_files(operation, out);
    }

    /// Everything about files is forgotten (sign-out).
    pub(super) fn clear_files(&mut self, out: &mut Output) {
        let ids: Vec<AttachmentId> = self.files.entries.keys().copied().collect();
        if !ids.is_empty() {
            out.effects.push(Effect::ReleaseFiles { attachments: ids });
        }
        for id in self
            .files
            .downloads
            .iter()
            .filter(|(_, state)| matches!(state, DownloadState::Downloading { .. }))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
        {
            out.effects.push(Effect::CancelDownload { attachment: id });
        }
        self.files = Files::default();
    }
}

// ----- Downloads -----

impl ClientCore {
    fn download_attachment(
        &mut self,
        attachment: Attachment,
        destination: PathBuf,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let id = attachment.id;
        if matches!(
            self.files.downloads.get(&id),
            Some(DownloadState::Downloading { .. })
        ) {
            return;
        }
        self.files.downloads.insert(
            id,
            DownloadState::Downloading {
                received: 0,
                total: attachment.size,
            },
        );
        self.files
            .download_names
            .insert(id, attachment.name.clone());
        out.effects.push(Effect::DownloadAttachment {
            attachment,
            destination,
        });
        out.changes.push(Change::DownloadChanged { attachment: id });
    }

    fn cancel_download(&mut self, attachment: AttachmentId, out: &mut Output) {
        if matches!(
            self.files.downloads.get(&attachment),
            Some(DownloadState::Downloading { .. })
        ) {
            self.files.downloads.remove(&attachment);
            self.files.download_names.remove(&attachment);
            out.effects.push(Effect::CancelDownload { attachment });
            out.changes.push(Change::DownloadChanged { attachment });
        }
    }

    fn dismiss_download(&mut self, attachment: AttachmentId, out: &mut Output) {
        if matches!(
            self.files.downloads.get(&attachment),
            Some(DownloadState::Done { .. } | DownloadState::Failed { .. })
        ) {
            self.files.downloads.remove(&attachment);
            self.files.download_names.remove(&attachment);
            out.changes.push(Change::DownloadChanged { attachment });
        }
    }

    fn download_progress(
        &mut self,
        attachment: AttachmentId,
        received: u64,
        total: u64,
        out: &mut Output,
    ) {
        let Some(DownloadState::Downloading {
            received: before,
            total: known,
        }) = self.files.downloads.get(&attachment)
        else {
            return;
        };
        let total = total.max(*known);
        let received = received.min(total);
        if received <= *before {
            return;
        }
        self.files
            .downloads
            .insert(attachment, DownloadState::Downloading { received, total });
        out.changes.push(Change::DownloadChanged { attachment });
    }

    fn download_result(
        &mut self,
        attachment: AttachmentId,
        result: Result<PathBuf, TransferError>,
        out: &mut Output,
    ) {
        if !matches!(
            self.files.downloads.get(&attachment),
            Some(DownloadState::Downloading { .. })
        ) {
            // Cancelled meanwhile.
            return;
        }
        let name = self
            .files
            .download_names
            .remove(&attachment)
            .unwrap_or_else(|| "The file".to_owned());
        let state = match result {
            Ok(path) => DownloadState::Done { path },
            Err(TransferError::Request(error)) if error.is_unauthenticated() => {
                self.files.downloads.remove(&attachment);
                out.changes.push(Change::DownloadChanged { attachment });
                self.session_expired(out);
                return;
            }
            Err(error) => DownloadState::Failed {
                reason: download_failure(&name, &error),
            },
        };
        self.files.downloads.insert(attachment, state);
        out.changes.push(Change::DownloadChanged { attachment });
    }
}

fn download_failure(name: &str, error: &TransferError) -> String {
    match error {
        TransferError::Request(RequestError::Rejected(api)) => match api.code {
            ErrorCode::NotFound => format!("{name} is no longer available."),
            ErrorCode::Forbidden => format!("You don't have access to {name}."),
            _ => format!("Couldn't download {name}. {}", api.message),
        },
        TransferError::Request(RequestError::Unknown(_)) => {
            format!("Couldn't download {name}. Check your connection and try again.")
        }
        TransferError::Disk(why) | TransferError::Unreadable(why) => {
            format!("Couldn't save {name}: {}.", why.trim_end_matches('.'))
        }
        TransferError::ChecksumMismatch => {
            format!("{name} didn't download correctly (its checksum doesn't match). Try again.")
        }
        TransferError::TooLarge { .. } | TransferError::Missing => {
            format!("Couldn't download {name}.")
        }
        TransferError::Cancelled => "Cancelled".to_owned(),
    }
}

// ----- Images -----

impl ClientCore {
    fn request_image(&mut self, attachment: AttachmentId, full: bool, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let held = if full {
            self.files.full_images.get(&attachment)
        } else {
            self.files.thumbnails.get(&attachment)
        };
        match held {
            Some(ImageState::Ready(_)) => {
                // Used again: it is the last to be dropped.
                let order = if full {
                    &mut self.files.full_order
                } else {
                    &mut self.files.thumbnail_order
                };
                order.retain(|id| *id != attachment);
                order.push_back(attachment);
                return;
            }
            Some(ImageState::Loading) => return,
            // A thumbnail is asked for whenever a row is drawn, so a failed one waits for the
            // connection to come back. A whole image is asked for by the person, so asking
            // again ("Try again" in the viewer) is a new request.
            Some(ImageState::Failed) if !full => return,
            Some(ImageState::Failed) | None => {}
        }
        if full {
            self.files
                .full_images
                .insert(attachment, ImageState::Loading);
        } else {
            self.files
                .thumbnails
                .insert(attachment, ImageState::Loading);
        }
        self.files.image_queue.push_back((attachment, full));
        // A long line of requests for rows that have scrolled away drops the oldest.
        while self.files.image_queue.len() > MAX_QUEUED_IMAGE_FETCHES {
            if let Some((dropped, dropped_full)) = self.files.image_queue.pop_front() {
                let map = if dropped_full {
                    &mut self.files.full_images
                } else {
                    &mut self.files.thumbnails
                };
                if matches!(map.get(&dropped), Some(ImageState::Loading)) {
                    map.remove(&dropped);
                }
            }
        }
        out.changes.push(Change::ImageChanged { attachment });
        self.pump_images(out);
    }

    fn pump_images(&mut self, out: &mut Output) {
        while self.files.image_in_flight.len() < MAX_CONCURRENT_IMAGE_FETCHES {
            let Some((attachment, full)) = self.files.image_queue.pop_front() else {
                break;
            };
            let state = if full {
                self.files.full_images.get(&attachment)
            } else {
                self.files.thumbnails.get(&attachment)
            };
            if !matches!(state, Some(ImageState::Loading)) {
                continue;
            }
            if self.files.image_in_flight.insert((attachment, full)) {
                out.effects.push(Effect::FetchImage { attachment, full });
            }
        }
    }

    fn image_loaded(
        &mut self,
        attachment: AttachmentId,
        full: bool,
        result: Result<ImageData, TransferError>,
        out: &mut Output,
    ) {
        self.files.image_in_flight.remove(&(attachment, full));
        let map = if full {
            &mut self.files.full_images
        } else {
            &mut self.files.thumbnails
        };
        if !matches!(map.get(&attachment), Some(ImageState::Loading)) {
            // Forgotten while it was on its way.
            self.pump_images(out);
            return;
        }
        match result {
            Ok(data) => {
                let len = data.len();
                map.insert(attachment, ImageState::Ready(data));
                if full {
                    self.files.full_order.push_back(attachment);
                    while self.files.full_order.len() > MAX_HELD_FULL_IMAGES {
                        if let Some(old) = self.files.full_order.pop_front()
                            && old != attachment
                        {
                            self.files.full_images.remove(&old);
                        }
                    }
                } else {
                    self.files.thumbnail_order.push_back(attachment);
                    self.files.thumbnail_bytes += len;
                    self.evict_thumbnails(attachment);
                }
            }
            Err(TransferError::Request(error)) if error.is_unauthenticated() => {
                map.remove(&attachment);
                self.session_expired(out);
            }
            Err(TransferError::Cancelled) => {
                map.remove(&attachment);
            }
            Err(_) => {
                map.insert(attachment, ImageState::Failed);
            }
        }
        out.changes.push(Change::ImageChanged { attachment });
        self.pump_images(out);
    }

    /// Drops the thumbnails used longest ago until the ones held fit their bounds. `keep` is
    /// the one that just arrived.
    fn evict_thumbnails(&mut self, keep: AttachmentId) {
        while (self.files.thumbnail_order.len() > MAX_HELD_THUMBNAILS
            || self.files.thumbnail_bytes > MAX_HELD_THUMBNAIL_BYTES)
            && self.files.thumbnail_order.len() > 1
        {
            let Some(old) = self.files.thumbnail_order.pop_front() else {
                break;
            };
            if old == keep {
                self.files.thumbnail_order.push_back(old);
                continue;
            }
            if let Some(ImageState::Ready(data)) = self.files.thumbnails.remove(&old) {
                self.files.thumbnail_bytes = self.files.thumbnail_bytes.saturating_sub(data.len());
            }
        }
    }

    fn release_full_image(&mut self, attachment: AttachmentId) {
        self.files.full_images.remove(&attachment);
        self.files.full_order.retain(|id| *id != attachment);
        self.files
            .image_queue
            .retain(|job| *job != (attachment, true));
    }

    /// The connection is back: images that could not be fetched are asked for again the next
    /// time they are wanted.
    pub(super) fn retry_failed_images(&mut self, out: &mut Output) {
        let mut retried = Vec::new();
        self.files.thumbnails.retain(|id, state| {
            let failed = *state == ImageState::Failed;
            if failed {
                retried.push(*id);
            }
            !failed
        });
        self.files.full_images.retain(|id, state| {
            let failed = *state == ImageState::Failed;
            if failed {
                retried.push(*id);
            }
            !failed
        });
        for attachment in retried {
            out.changes.push(Change::ImageChanged { attachment });
        }
    }

    /// The files of a deleted message can no longer be read: drop what is held for them.
    pub(super) fn forget_attachments(&mut self, ids: Vec<AttachmentId>, out: &mut Output) {
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            if let Some(ImageState::Ready(data)) = self.files.thumbnails.remove(id) {
                self.files.thumbnail_bytes = self.files.thumbnail_bytes.saturating_sub(data.len());
            }
            self.files.full_images.remove(id);
            self.files.thumbnail_order.retain(|held| held != id);
            self.files.full_order.retain(|held| held != id);
            self.files.image_queue.retain(|job| job.0 != *id);
            out.changes.push(Change::ImageChanged { attachment: *id });
        }
        out.effects
            .push(Effect::ForgetAttachments { attachments: ids });
    }

    /// The ids of the files on a message's row, wherever the core holds it.
    pub(super) fn attachment_ids_of_row(
        &self,
        conversation: ConversationId,
        message: lazy_domain::MessageId,
    ) -> Vec<AttachmentId> {
        self.find_row(conversation, message)
            .map(|row| row.attachments.iter().map(|a| a.id).collect())
            .unwrap_or_default()
    }

    /// Whether the document of a message the person is editing may be empty: it has files.
    pub(super) fn row_has_files(
        &self,
        conversation: ConversationId,
        message: lazy_domain::MessageId,
    ) -> bool {
        self.find_row(conversation, message)
            .is_some_and(|row| !row.attachments.is_empty())
    }

    /// The empty document a message with only files carries.
    pub(super) fn files_only_document() -> MessageDocument {
        MessageDocument::empty()
    }
}
