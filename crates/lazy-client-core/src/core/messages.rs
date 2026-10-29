//! Editing and deleting messages.
//!
//! # Edits
//!
//! An edit is a submission like a send (D12): it has an operation id, is journaled before the
//! request, and an unknown outcome is reconciled by asking the server, never by making a new
//! operation. What differs is concurrency, which is per message:
//!
//! * One edit of a message is in flight at a time. Text written meanwhile replaces the single
//!   queued edit (the latest text wins) and is sent, unjournaled, on top of the revision the
//!   first edit produced. A queued edit is not durable: it exists only in memory, because the
//!   first edit is still the journal's entry for the message.
//! * Every edit carries `base_revision`, the revision the editor saw. If the message moved on
//!   the server answers `stale_revision`; the core fetches the message, shows the newer version,
//!   and keeps the person's text as a conflict they can save over it or abandon.
//! * While an edit is saving, its row shows the new content and the server's content is kept
//!   aside in the timeline. Every stopped state shows the server's content again, so a row never
//!   claims text the server does not have.
//!
//! # Deletes
//!
//! Deleting is idempotent on the server, so it has no operation id: a lost response is simply
//! asked again. The row disappears when the answer or the event arrives, whichever is first.

use lazy_protocol::{EditMessageRequest, ErrorCode, Mentions, MessageResponse};

use super::conversations::readable_error;
use super::*;

const EDIT_LOST_TO_DELETION: &str = "That message was deleted, so your edit wasn't saved.";

/// Where the latest edit request of a message is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditStage {
    /// Waiting for the journal write to become durable.
    Persisting,
    /// Journaled and ready to send, but the session is not ready for requests.
    Queued,
    /// The edit request is in flight.
    Sending,
    /// A status query is in flight.
    Querying,
    /// The server said the base revision is stale; the current message is being fetched.
    Fetching,
    /// Not saved and not in flight; waiting for the person.
    Failed { reason: String, retry: RetryKind },
    /// The message changed first; the person's text is kept for them to save over it.
    Conflict,
}

impl EditStage {
    fn is_running(&self) -> bool {
        !matches!(self, Self::Failed { .. } | Self::Conflict)
    }

    /// Whether the operation may still have committed, so its journal entry must be kept.
    fn holds_operation(&self) -> bool {
        matches!(
            self,
            Self::Persisting
                | Self::Queued
                | Self::Sending
                | Self::Querying
                | Self::Failed {
                    retry: RetryKind::SameOperation,
                    ..
                }
        )
    }
}

/// The request being made for an edit, or the last one made.
#[derive(Debug)]
struct ActiveEdit {
    operation: OperationId,
    base_revision: u32,
    /// What the person wrote, as source text.
    text: String,
    document: MessageDocument,
    stage: EditStage,
    /// Resends triggered by "no record" answers since the person last retried.
    resends: u32,
}

/// Text written while a request was in flight.
#[derive(Debug)]
struct QueuedEdit {
    text: String,
    document: MessageDocument,
}

/// The edits of one message that have not reached a final state.
#[derive(Debug)]
pub(super) struct EditEntry {
    conversation: ConversationId,
    active: ActiveEdit,
    /// Only while `active` is running, or failed with an unknown outcome.
    queued: Option<QueuedEdit>,
}

impl EditEntry {
    /// What the message will say once the edits in progress finish, if any are.
    fn intended(&self) -> Option<&MessageDocument> {
        match &self.queued {
            Some(queued) => Some(&queued.document),
            None if self.active.stage.is_running() => Some(&self.active.document),
            None => None,
        }
    }

    /// The text the person wrote last.
    fn person_text(&self) -> &str {
        self.queued
            .as_ref()
            .map_or(self.active.text.as_str(), |queued| queued.text.as_str())
    }

    /// What the row shows: the edit state, and the content to display while saving.
    fn row_state(&self) -> (EditState, Option<MessageDocument>) {
        match &self.active.stage {
            EditStage::Failed { reason, retry } => (
                EditState::Failed {
                    reason: reason.clone(),
                    text: self.person_text().to_owned(),
                    retry: *retry,
                },
                None,
            ),
            EditStage::Conflict => (
                EditState::Conflict {
                    text: self.person_text().to_owned(),
                },
                None,
            ),
            _ => (EditState::Saving, self.intended().cloned()),
        }
    }
}

// ----- What to offer -----

impl ClientCore {
    /// Whether to offer "Edit" on a message: the author's own, in a conversation they can still
    /// post in. The server decides; this only avoids offering the impossible.
    pub fn can_edit_message(&self, conversation: ConversationId, message: MessageId) -> bool {
        self.message_facts(conversation, message)
            .is_some_and(|facts| facts.can_edit())
    }

    /// Whether to offer "Delete" on a message: the author's own, or anyone's for an admin or
    /// owner of the workspace.
    pub fn can_delete_message(&self, conversation: ConversationId, message: MessageId) -> bool {
        self.message_facts(conversation, message)
            .is_some_and(|facts| facts.can_delete())
    }

    /// The text to start editing a message from: what the person last wrote if an edit is
    /// waiting, saving, failed, or conflicted, otherwise the message's current content as
    /// source. `None` if the message is not loaded.
    pub fn edit_text(&self, conversation: ConversationId, message: MessageId) -> Option<String> {
        let row = self.find_row(conversation, message)?;
        Some(match self.edits.get(&message) {
            Some(entry) => entry.person_text().to_owned(),
            None => row.document.to_markdown(),
        })
    }
}

// ----- Editing -----

impl ClientCore {
    pub(super) fn edit_message(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        text: &str,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(facts) = self.message_facts(conversation, message) else {
            return;
        };
        if !facts.can_edit() {
            let notice = if facts.is_author {
                "This conversation is read-only, so its messages can't be edited."
            } else {
                "You can only edit your own messages."
            };
            self.set_notice(Some(notice.to_owned()), out);
            return;
        }
        let document = match parse_composer(text, &self.mention_directory(conversation)) {
            Ok(document) => document,
            // A message with files may lose its words: the files are the message.
            Err(DocumentError::Empty) if self.row_has_files(conversation, message) => {
                Self::files_only_document()
            }
            Err(DocumentError::Empty) => {
                let notice = "A message can't be empty. Delete it instead.";
                self.set_notice(Some(notice.to_owned()), out);
                return;
            }
            Err(error) => {
                self.set_notice(Some(format!("Edit not saved: {error}.")), out);
                return;
            }
        };
        let Some(saved) = self.saved_document_of(conversation, message).cloned() else {
            return;
        };
        let queued = QueuedEdit {
            text: text.to_owned(),
            document: document.clone(),
        };
        match self
            .edits
            .get(&message)
            .map(|entry| entry.active.stage.clone())
        {
            None => {
                if document != saved {
                    self.begin_edit(conversation, message, queued, out);
                }
            }
            // The earlier attempt may have committed. Settle it first; this text goes on top.
            Some(EditStage::Failed {
                retry: RetryKind::SameOperation,
                ..
            }) => {
                if let Some(entry) = self.edits.get_mut(&message) {
                    entry.queued = Some(queued);
                }
                self.requery_edit(message, true, out);
            }
            // Nothing is outstanding on the server: this is a fresh edit of the message.
            Some(EditStage::Failed { .. } | EditStage::Conflict) => {
                self.drop_edit(conversation, message, out);
                if document != saved {
                    self.begin_edit(conversation, message, queued, out);
                }
            }
            Some(_) => {
                let Some(entry) = self.edits.get_mut(&message) else {
                    return;
                };
                if entry.intended() == Some(&document) {
                    return;
                }
                entry.queued = Some(queued);
                self.sync_edit_row(conversation, message, out);
            }
        }
    }

    /// Starts a new operation to edit `message` on top of the revision the timeline holds: the
    /// row shows the new content and the journal write begins.
    fn begin_edit(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        edit: QueuedEdit,
        out: &mut Output,
    ) {
        let Some(me) = self.my_id() else {
            return;
        };
        let Some(base_revision) = self
            .find_row(conversation, message)
            .and_then(TimelineItem::revision)
        else {
            return;
        };
        let operation = OperationId::generate();
        self.edit_ops.insert(operation, message);
        self.edits.insert(
            message,
            EditEntry {
                conversation,
                active: ActiveEdit {
                    operation,
                    base_revision,
                    text: edit.text,
                    document: edit.document.clone(),
                    stage: EditStage::Persisting,
                    resends: 0,
                },
                queued: None,
            },
        );
        self.sync_edit_row(conversation, message, out);
        out.effects.push(Effect::PersistSubmission(JournalEntry {
            operation_id: operation,
            user_id: me,
            conversation_id: conversation,
            document: edit.document,
            submitted_at: Timestamp::now(),
            kind: JournalKind::Edit {
                message_id: message,
                base_revision,
            },
            thread_root: None,
            also_in_channel: false,
            attachments: Vec::new(),
        }));
    }

    pub(super) fn cancel_edit(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        let Some(entry) = self.edits.get_mut(&message) else {
            return;
        };
        if entry.conversation != conversation {
            return;
        }
        if entry.active.stage.is_running() {
            // A request already sent cannot be taken back; only the waiting text can.
            if entry.queued.take().is_some() {
                self.sync_edit_row(conversation, message, out);
            }
            return;
        }
        self.drop_edit(conversation, message, out);
    }

    pub(super) fn retry_edit(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(entry) = self.edits.get(&message) else {
            return;
        };
        if entry.conversation != conversation {
            return;
        }
        match entry.active.stage {
            EditStage::Failed {
                retry: RetryKind::SameOperation,
                ..
            } => self.requery_edit(message, true, out),
            EditStage::Failed { .. } | EditStage::Conflict => {
                let text = entry.person_text().to_owned();
                self.drop_edit(conversation, message, out);
                // The text was valid when it was written, so only a bug could fail here.
                if let Ok(document) = parse_composer(&text, &self.mention_directory(conversation)) {
                    let unchanged = self
                        .saved_document_of(conversation, message)
                        .is_some_and(|saved| *saved == document);
                    if !unchanged {
                        self.begin_edit(conversation, message, QueuedEdit { text, document }, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Asks the server what became of the latest operation of a message's edit.
    fn requery_edit(&mut self, message: MessageId, reset_resends: bool, out: &mut Output) {
        let Some(entry) = self.edits.get_mut(&message) else {
            return;
        };
        entry.active.stage = EditStage::Querying;
        if reset_resends {
            entry.active.resends = 0;
        }
        let (operation, conversation) = (entry.active.operation, entry.conversation);
        out.effects.push(Effect::QueryOperationStatus { operation });
        self.sync_edit_row(conversation, message, out);
    }

    /// Sends the edit request if it is waiting for the session.
    fn send_edit(&mut self, message: MessageId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(entry) = self.edits.get_mut(&message) else {
            return;
        };
        if entry.active.stage != EditStage::Queued {
            return;
        }
        entry.active.stage = EditStage::Sending;
        out.effects.push(Effect::EditMessage {
            message,
            request: EditMessageRequest {
                operation_id: entry.active.operation,
                base_revision: entry.active.base_revision,
                document: entry.active.document.clone(),
            },
        });
    }

    /// Sends every edit that was waiting for the session.
    pub(super) fn pump_edits(&mut self, out: &mut Output) {
        let waiting: Vec<MessageId> = self
            .edits
            .iter()
            .filter(|(_, entry)| entry.active.stage == EditStage::Queued)
            .map(|(message, _)| *message)
            .collect();
        for message in waiting {
            self.send_edit(message, out);
        }
    }

    pub(super) fn edit_persisted(&mut self, operation: OperationId, out: &mut Output) {
        let Some(&message) = self.edit_ops.get(&operation) else {
            return;
        };
        if let Some(entry) = self.edits.get_mut(&message)
            && entry.active.operation == operation
            && entry.active.stage == EditStage::Persisting
        {
            entry.active.stage = EditStage::Queued;
            self.send_edit(message, out);
        }
    }

    pub(super) fn edit_persist_failed(
        &mut self,
        operation: OperationId,
        reason: &str,
        out: &mut Output,
    ) {
        let Some(&message) = self.edit_ops.get(&operation) else {
            return;
        };
        if self
            .edits
            .get(&message)
            .is_some_and(|entry| entry.active.stage == EditStage::Persisting)
        {
            let reason = format!("Couldn't save the edit before sending: {reason}");
            self.fail_edit(message, reason, RetryKind::NewOperation, out);
        }
    }

    pub(super) fn edit_result(
        &mut self,
        operation: OperationId,
        result: Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(&message) = self.edit_ops.get(&operation) else {
            return;
        };
        if !self.edits.get(&message).is_some_and(|entry| {
            entry.active.operation == operation && entry.active.stage == EditStage::Sending
        }) {
            // Already settled by the live event, or superseded.
            return;
        }
        match result {
            Ok(response) => self.edit_committed(message, response.message, out),
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(RequestError::Rejected(error)) => self.edit_rejected(message, error, out),
            Err(RequestError::Unknown(_)) => self.requery_edit(message, false, out),
        }
    }

    /// The server definitively refused the edit.
    fn edit_rejected(
        &mut self,
        message: MessageId,
        error: lazy_protocol::ApiError,
        out: &mut Output,
    ) {
        let Some(entry) = self.edits.get_mut(&message) else {
            return;
        };
        let (operation, conversation) = (entry.active.operation, entry.conversation);
        let document = entry.active.document.clone();
        match error.code {
            ErrorCode::StaleRevision => {
                // Not committed, so the journal entry is done. Find out what the message says now.
                entry.active.stage = EditStage::Fetching;
                self.edit_ops.remove(&operation);
                out.effects.push(Effect::SettleSubmission { operation });
                out.effects.push(Effect::FetchMessage { message });
                self.sync_edit_row(conversation, message, out);
            }
            ErrorCode::Forbidden | ErrorCode::InvalidRequest
                if mention_refusal(Some(&document), &error).is_some() =>
            {
                // The words are the problem, not the message: the person's text is kept to fix.
                let reason = mention_refusal(Some(&document), &error).unwrap_or_default();
                self.fail_edit(message, reason, RetryKind::NewOperation, out);
            }
            ErrorCode::Forbidden | ErrorCode::NotFound => {
                // The message is gone or no longer the person's to edit: there is nothing to
                // retry, so the row goes back to what the server has.
                self.drop_edit(conversation, message, out);
                let reason = if error.code == ErrorCode::Forbidden {
                    "You're not allowed to edit that message."
                } else {
                    "That message no longer exists."
                };
                self.set_notice(Some(format!("Your edit wasn't saved. {reason}")), out);
            }
            _ => {
                let reason = if error.message.is_empty() {
                    readable_error(&RequestError::Rejected(error))
                } else {
                    error.message
                };
                self.fail_edit(message, reason, RetryKind::NewOperation, out);
            }
        }
    }

    pub(super) fn edit_status_result(
        &mut self,
        message: MessageId,
        operation: OperationId,
        result: Result<OperationStatus, RequestError>,
        out: &mut Output,
    ) {
        let Some(entry) = self.edits.get(&message) else {
            return;
        };
        if entry.active.operation != operation {
            return;
        }
        // "It committed" is true whatever the edit is doing now (an answer overtaken by a resend,
        // for example), so it is accepted at any stage that still holds the operation.
        if let Ok(OperationStatus::EditCommitted { message: committed }) = result {
            if entry.active.stage.holds_operation() {
                self.edit_committed(message, committed, out);
            }
            return;
        }
        if entry.active.stage != EditStage::Querying {
            return;
        }
        match result {
            Ok(OperationStatus::NotFound) => self.edit_not_found(message, out),
            Ok(_) => {
                let reason = "The server gave an unexpected answer.".to_owned();
                self.fail_edit(message, reason, RetryKind::SameOperation, out);
            }
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(error) => {
                let reason = match error {
                    RequestError::Rejected(error) => error.message,
                    RequestError::Unknown(_) => {
                        "Couldn't confirm whether this edit was saved.".to_owned()
                    }
                };
                self.fail_edit(message, reason, RetryKind::SameOperation, out);
            }
        }
    }

    /// The server has no record of the edit's operation.
    fn edit_not_found(&mut self, message: MessageId, out: &mut Output) {
        let Some(entry) = self.edits.get_mut(&message) else {
            return;
        };
        let (operation, conversation) = (entry.active.operation, entry.conversation);
        if let Some(queued) = entry.queued.take() {
            // The person has written something newer since, so the old text is not resent. If
            // that request still reaches the server it carries a base revision the new edit
            // will have outdated, and is rejected as stale.
            self.edit_ops.remove(&operation);
            self.edits.remove(&message);
            out.effects.push(Effect::SettleSubmission { operation });
            self.begin_edit(conversation, message, queued, out);
            self.sync_edit_row(conversation, message, out);
            return;
        }
        if entry.active.resends >= MAX_RESENDS {
            let reason = "The server never confirmed this edit.".to_owned();
            self.fail_edit(message, reason, RetryKind::SameOperation, out);
            return;
        }
        // Resending the same operation is idempotent.
        entry.active.resends += 1;
        entry.active.stage = EditStage::Queued;
        self.send_edit(message, out);
        self.sync_edit_row(conversation, message, out);
    }

    /// The edit committed (or its operation did) as `committed`: the row takes the new revision,
    /// the journal entry is released, and an edit that was waiting goes next. Safe to call more
    /// than once for the same operation.
    fn edit_committed(&mut self, message: MessageId, committed: Message, out: &mut Output) {
        let Some(entry) = self.edits.get(&message) else {
            return;
        };
        let conversation = entry.conversation;
        if committed.conversation_id != conversation || committed.id != message {
            return;
        }
        if committed.deleted {
            // The message is gone: nothing was left to edit, and the pending edit is moot.
            self.remove_deleted(&committed, out);
            return self.set_notice(Some(EDIT_LOST_TO_DELETION.to_owned()), out);
        }
        self.each_home(conversation, out, |_, timeline, changes| {
            timeline.apply_update(conversation, &committed, changes);
        });
        self.cache_changed(false);
        let Some(entry) = self.edits.remove(&message) else {
            return;
        };
        let operation = entry.active.operation;
        self.edit_ops.remove(&operation);
        out.effects.push(Effect::SettleSubmission { operation });
        match entry.queued {
            // An edit that leaves the message as it is needs no request.
            Some(queued) if queued.document != committed.document => {
                self.begin_edit(conversation, message, queued, out);
            }
            _ => {}
        }
        self.sync_edit_row(conversation, message, out);
    }

    /// Marks the latest request of an edit failed. `retry` says what retrying does; a definitive
    /// failure releases the journal entry and folds any waiting text into the failed one, since
    /// it is the latest thing the person wrote.
    fn fail_edit(
        &mut self,
        message: MessageId,
        reason: String,
        retry: RetryKind,
        out: &mut Output,
    ) {
        let Some(entry) = self.edits.get_mut(&message) else {
            return;
        };
        let operation = entry.active.operation;
        let held = entry.active.stage.holds_operation();
        entry.active.stage = EditStage::Failed { reason, retry };
        if retry == RetryKind::NewOperation
            && let Some(queued) = entry.queued.take()
        {
            entry.active.text = queued.text;
            entry.active.document = queued.document;
        }
        let conversation = entry.conversation;
        if retry == RetryKind::NewOperation && held {
            self.edit_ops.remove(&operation);
            out.effects.push(Effect::SettleSubmission { operation });
        }
        self.sync_edit_row(conversation, message, out);
    }

    /// Forgets the edits of a message, releasing a journal entry that is still held, and shows
    /// the row as the server has it.
    pub(super) fn drop_edit(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        if let Some(entry) = self.edits.remove(&message)
            && entry.active.stage.holds_operation()
        {
            let operation = entry.active.operation;
            self.edit_ops.remove(&operation);
            if entry.active.stage == EditStage::Persisting {
                // The write may still land after a release, which would resurrect the entry.
                self.abandoned_persists.insert(operation);
            } else {
                out.effects.push(Effect::SettleSubmission { operation });
            }
        }
        self.sync_edit_row(conversation, message, out);
    }

    /// Forgets every edit in a conversation that is no longer here.
    pub(super) fn drop_edits_in(&mut self, conversation: ConversationId, out: &mut Output) {
        let doomed: Vec<MessageId> = self
            .edits
            .iter()
            .filter(|(_, entry)| entry.conversation == conversation)
            .map(|(message, _)| *message)
            .collect();
        for message in doomed {
            self.drop_edit(conversation, message, out);
        }
    }

    /// Makes the row of `message` show what its edit entry says, or nothing if it has none.
    pub(super) fn sync_edit_row(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        let (state, saving) = match self.edits.get(&message) {
            Some(entry) => entry.row_state(),
            None => (EditState::None, None),
        };
        self.each_home(conversation, out, |_, timeline, changes| {
            timeline.set_edit(
                conversation,
                message,
                state.clone(),
                saving.as_ref(),
                changes,
            );
        });
    }

    /// Re-applies edit states to the rows of a conversation, for rows that were just loaded.
    pub(super) fn resync_edit_rows(&mut self, conversation: ConversationId, out: &mut Output) {
        let messages: Vec<MessageId> = self
            .edits
            .iter()
            .filter(|(_, entry)| entry.conversation == conversation)
            .map(|(message, _)| *message)
            .collect();
        for message in messages {
            self.sync_edit_row(conversation, message, out);
        }
    }

    /// The connection is back: asks again about edits whose outcome is unknown and that nothing
    /// on screen can retry, because the message is not loaded (an edit recovered from the
    /// journal after a restart, or one whose conversation was reloaded since). A message that is
    /// on screen shows the failure with a retry instead.
    pub(super) fn retry_unseen_edits(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let stalled: Vec<MessageId> = self
            .edits
            .iter()
            .filter(|(message, entry)| {
                matches!(
                    entry.active.stage,
                    EditStage::Failed {
                        retry: RetryKind::SameOperation,
                        ..
                    }
                ) && self.find_row(entry.conversation, **message).is_none()
            })
            .map(|(message, _)| *message)
            .collect();
        for message in stalled {
            self.requery_edit(message, true, out);
        }
    }

    /// The session ended: edits in progress cannot continue until the person signs in again.
    pub(super) fn fail_running_edits(&mut self, out: &mut Output) {
        let running: Vec<(MessageId, bool)> = self
            .edits
            .iter()
            .filter(|(_, entry)| entry.active.stage.is_running())
            .map(|(message, entry)| (*message, entry.active.stage.holds_operation()))
            .collect();
        for (message, held) in running {
            let reason = "Signed out before this edit was saved.".to_owned();
            let retry = if held {
                RetryKind::SameOperation
            } else {
                RetryKind::NewOperation
            };
            self.fail_edit(message, reason, retry, out);
        }
    }

    /// Shows a journaled edit from a previous run as being saved, and asks the server about it
    /// before anything is resent.
    pub(super) fn recover_edit(
        &mut self,
        entry: JournalEntry,
        message: MessageId,
        base_revision: u32,
        out: &mut Output,
    ) {
        let operation = entry.operation_id;
        let conversation = entry.conversation_id;
        // The conversation is gone, or the message already has an edit being reconciled (the
        // journal never holds two for one message): either way this entry has nothing to do.
        if self.conversation(conversation).is_none() || self.edits.contains_key(&message) {
            out.effects.push(Effect::SettleSubmission { operation });
            return;
        }
        self.edit_ops.insert(operation, message);
        self.edits.insert(
            message,
            EditEntry {
                conversation,
                active: ActiveEdit {
                    operation,
                    base_revision,
                    text: entry.document.to_markdown(),
                    document: entry.document,
                    stage: EditStage::Querying,
                    resends: 0,
                },
                queued: None,
            },
        );
        out.effects.push(Effect::QueryOperationStatus { operation });
        self.sync_edit_row(conversation, message, out);
    }

    // ----- Events -----

    pub(super) fn apply_message_edited(
        &mut self,
        message: Message,
        operation: Option<OperationId>,
        previous_mentions: &Mentions,
        seq: EventSeq,
        out: &mut Output,
    ) {
        if self.conversation(message.conversation_id).is_none() {
            return;
        }
        self.count_message_edited(&message, previous_mentions, seq, out);
        self.count_reply_edited(&message, previous_mentions, seq, out);
        if !message.deleted {
            self.search_message_edited(&message, out);
            self.inbox_message_edited(&message, out);
            self.discovery_message_edited(&message, out);
        }
        if let Some(own) = operation.and_then(|op| self.edit_ops.get(&op).copied()) {
            return self.edit_committed(own, message, out);
        }
        // Someone else's edit, the person's own from another device, or one already settled.
        let conversation = message.conversation_id;
        if message.deleted {
            // A replay serves the earlier events of a deleted message as tombstones. Whatever
            // the client holds waits for the deletion event that follows; nothing is added.
            self.each_home(conversation, out, |_, timeline, _| {
                timeline.note_unloaded_update(&message);
            });
            return;
        }
        let mut changed = false;
        self.each_home(conversation, out, |_, timeline, changes| {
            changed |= timeline.apply_update(conversation, &message, changes);
        });
        if changed {
            self.cache_changed(false);
        }
    }

    pub(super) fn apply_message_deleted(
        &mut self,
        message: Message,
        had_mentions: &Mentions,
        seq: EventSeq,
        out: &mut Output,
    ) {
        if self.conversation(message.conversation_id).is_none() {
            return;
        }
        self.count_message_deleted(&message, had_mentions, seq, out);
        self.count_reply_deleted(&message, had_mentions, seq, out);
        self.remove_deleted_at(&message, Some(seq), out);
    }

    /// The message is gone: its row, its edits, and anything waiting on it go with it.
    pub(super) fn remove_deleted(&mut self, message: &Message, out: &mut Output) {
        self.remove_deleted_at(message, None, out);
    }

    /// Like [`ClientCore::remove_deleted`]. `at` is the position of the deletion event, which is
    /// the version of the summary the tombstone of a root carries.
    pub(super) fn remove_deleted_at(
        &mut self,
        message: &Message,
        at: Option<EventSeq>,
        out: &mut Output,
    ) {
        let conversation = message.conversation_id;
        let reply = message.thread_root.is_some();
        let mut root_survives = false;
        let mut thread_ended = false;
        // A deleted message is no longer a search result, nor a notification, nor a thread's
        // newest reply.
        self.search_message_deleted(message.id, out);
        self.notify_message_deleted(message.id, out);
        self.inbox_message_deleted(message, out);
        self.discovery_message_deleted(message, out);
        // The files of a deleted message can no longer be read: forget previews and downloads.
        let mut files = self.attachment_ids_of_row(conversation, message.id);
        files.extend(message.attachments.iter().map(|file| file.id));
        files.sort();
        files.dedup();
        self.forget_attachments(files, out);
        self.each_home(conversation, out, |home, timeline, changes| {
            match (home, reply) {
                // A root that still has replies stays as a placeholder.
                (Home::Channel(_) | Home::PaneRoot, false) => {
                    let outcome = timeline.tombstone_root(conversation, message, changes);
                    root_survives |= home == Home::PaneRoot && outcome == DeletedRoot::Tombstoned;
                    // A root with no replies left takes its thread with it.
                    thread_ended |= matches!(home, Home::Channel(_))
                        && (outcome == DeletedRoot::Removed
                            || (outcome == DeletedRoot::Absent && message.thread.is_none()));
                    if outcome != DeletedRoot::Tombstoned {
                        // A history page that was requested before the deletion must not bring
                        // it back, nor the summary it read.
                        timeline.note_unloaded_update(message);
                        if let Some(at) = at {
                            timeline.note_unloaded_thread(message.id, at, message.thread.clone());
                        }
                    }
                }
                (Home::Channel(_) | Home::PaneReplies, true) => {
                    if let Some(index) = timeline.index_of_message(message.id) {
                        let key = timeline.items()[index].key;
                        timeline.remove(conversation, key, changes);
                    }
                    timeline.note_unloaded_update(message);
                }
                _ => {}
            }
        });
        if !reply
            && !root_survives
            && self
                .pane
                .as_ref()
                .is_some_and(|pane| pane.root == message.id)
        {
            // The thread had no replies left to keep its first message.
            self.close_pane(out);
            self.set_notice(
                Some("The message that started that thread was deleted.".to_owned()),
                out,
            );
        }
        if thread_ended {
            // Nothing on screen can reach the thread's composer or its failed replies now, so
            // their copies are not kept.
            self.release_files_of_thread(message.id, out);
        }
        self.drop_edit(conversation, message.id, out);
        self.drop_reactions_of(message.id);
        self.set_action(
            ActionKind::DeleteMessage(message.id),
            ActionState::Idle,
            out,
        );
        self.permalink_message_deleted(message.id, out);
        self.cache_changed(false);
    }
}

// ----- Fetching and deleting -----

impl ClientCore {
    pub(super) fn message_fetched(
        &mut self,
        message: MessageId,
        result: Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        self.edit_fetched(message, &result, out);
        self.pane_root_fetched(message, &result, out);
        self.permalink_fetched(message, result, out);
    }

    /// The answer to the fetch that follows a stale edit: show the newer version and keep the
    /// person's text as a conflict.
    fn edit_fetched(
        &mut self,
        message: MessageId,
        result: &Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(entry) = self.edits.get(&message) else {
            return;
        };
        if entry.active.stage != EditStage::Fetching {
            return;
        }
        let conversation = entry.conversation;
        match result {
            Ok(response) if response.message.deleted => {
                self.remove_deleted(&response.message, out);
                self.set_notice(Some(EDIT_LOST_TO_DELETION.to_owned()), out);
            }
            Ok(response) if response.message.conversation_id == conversation => {
                self.each_home(conversation, out, |_, timeline, changes| {
                    timeline.apply_update(conversation, &response.message, changes);
                });
                self.cache_changed(false);
                if let Some(entry) = self.edits.get_mut(&message) {
                    entry.active.stage = EditStage::Conflict;
                    if let Some(queued) = entry.queued.take() {
                        entry.active.text = queued.text;
                        entry.active.document = queued.document;
                    }
                }
                self.sync_edit_row(conversation, message, out);
                let notice = "This message was changed while you were editing it. Your text is \
                              kept; save it to replace the newer version.";
                self.set_notice(Some(notice.to_owned()), out);
            }
            Ok(_) => {}
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(_) => {
                let reason = "This message changed while you were editing it, and the newer \
                              version couldn't be loaded."
                    .to_owned();
                self.fail_edit(message, reason, RetryKind::NewOperation, out);
            }
        }
    }

    pub(super) fn delete_message(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if !self.can_delete_message(conversation, message) {
            if self.message_facts(conversation, message).is_some() {
                let notice = "You can't delete that message.";
                self.set_notice(Some(notice.to_owned()), out);
            }
            return;
        }
        if self.begin_action(ActionKind::DeleteMessage(message), out) {
            out.effects.push(Effect::DeleteMessage { message });
        }
    }

    pub(super) fn delete_result(
        &mut self,
        message: MessageId,
        result: Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        let kind = ActionKind::DeleteMessage(message);
        if self.action(kind) != &ActionState::InFlight {
            // The deletion event got here first and already removed the row.
            return;
        }
        match result {
            Ok(response) if response.message.deleted && response.message.id == message => {
                // The unread count follows the event, which is delivered in log order.
                self.remove_deleted(&response.message, out);
            }
            Ok(_) => {
                self.fail_action(kind, "The server gave an unexpected answer.", out);
            }
            Err(error) if error.is_unauthenticated() => {
                self.set_action(kind, ActionState::Idle, out);
                self.session_expired(out);
            }
            Err(RequestError::Rejected(error)) if error.code == ErrorCode::NotFound => {
                // Gone already, or no longer visible: either way it is not on screen any more.
                self.set_action(kind, ActionState::Idle, out);
                let located = self.conversation_of_row(message);
                if let Some(conversation) = located {
                    let files = self.attachment_ids_of_row(conversation, message);
                    self.forget_attachments(files, out);
                    self.each_home(conversation, out, |_, timeline, changes| {
                        if let Some(index) = timeline.index_of_message(message) {
                            let key = timeline.items()[index].key;
                            timeline.remove(conversation, key, changes);
                        }
                    });
                    self.drop_edit(conversation, message, out);
                    self.cache_changed(false);
                }
            }
            Err(error) => {
                let reason = readable_error(&error);
                self.fail_action(kind, reason.clone(), out);
                self.set_notice(Some(format!("Couldn't delete the message. {reason}")), out);
            }
        }
    }

    pub(super) fn set_jump(&mut self, target: Option<JumpTarget>, out: &mut Output) {
        if self.jump != target || target.is_some() {
            self.jump = target;
            out.changes.push(Change::JumpTargetChanged);
        }
    }
}
