//! The thread inbox (slice 3D, D20): the threads the person follows, most recently active first.
//!
//! One request is in flight at a time and every request has a generation, as for search: an answer
//! is shown only if it answers the request made last. Entries follow the events for the threads
//! they show; when an event says something the entries cannot absorb (a thread that is not listed
//! became followed or active), the first page is asked for again, quietly.

use lazy_domain::{ConversationId, EventSeq, MessageId};
use lazy_protocol::{
    ErrorCode, InboxEntry, InboxPage, InboxQuery, Message, ThreadSummary, ThreadUserState,
};

use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{RequestError, SessionPhase};
use crate::notify::{INBOX_PAGE, INBOX_READ_CAP, InboxStatus, InboxView};

use super::ClientCore;

/// The most "mark all read" requests one click may make.
const MAX_READ_ROUNDS: u32 = 20;

#[derive(Debug)]
struct Request {
    generation: u64,
    /// The first page, replacing the entries (otherwise the next one, appended).
    first: bool,
}

#[derive(Debug, Default)]
pub(super) struct Inboxing {
    view: Option<InboxView>,
    /// Only goes up, and survives closing the view.
    last_generation: u64,
    in_flight: Option<Request>,
    /// Something happened to the threads while a request was out: its answer may be out of date.
    stale: bool,
    read_rounds: u32,
}

impl Inboxing {
    /// Exchanges what belongs to one workspace and leaves the generation counter, which is shared.
    pub(super) fn swap_scoped(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.view, &mut other.view);
        std::mem::swap(&mut self.in_flight, &mut other.in_flight);
        std::mem::swap(&mut self.stale, &mut other.stale);
        std::mem::swap(&mut self.read_rounds, &mut other.read_rounds);
    }
}

impl ClientCore {
    /// The thread inbox, once opened with `Input::OpenInbox`.
    pub fn inbox(&self) -> Option<&InboxView> {
        self.inboxing.view.as_ref()
    }

    /// Unread replies across the threads the person follows, for the sidebar entry.
    pub fn inbox_unread_total(&self) -> u32 {
        self.followed_thread_unread_total()
    }

    /// Where an entry's thread is: `#design`, or the other people's names for a conversation.
    pub fn inbox_entry_title(&self, entry: &InboxEntry) -> String {
        let title = self.title_of(&entry.conversation);
        match entry.conversation.kind {
            lazy_domain::ConversationKind::Channel => format!("#{title}"),
            _ => title,
        }
    }

    /// Whether the person is in the conversation of an entry.
    pub fn inbox_entry_joined(&self, entry: &InboxEntry) -> bool {
        self.conversation(entry.conversation.id).is_some()
    }

    pub(super) fn inbox_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::OpenInbox => self.open_inbox(out),
            Input::LoadInbox { unread_only } => self.load_inbox(unread_only, out),
            Input::LoadMoreInbox => self.load_more_inbox(out),
            Input::MarkInboxRead => self.mark_inbox_read(out),
            Input::CloseInbox => self.close_inbox(out),
            Input::InboxLoaded { generation, result } => self.inbox_loaded(generation, result, out),
            Input::InboxReadResult { result } => self.inbox_read_result(result, out),
            _ => {}
        }
    }

    fn ready(&self) -> bool {
        matches!(self.phase, SessionPhase::Ready)
    }

    pub(super) fn open_inbox(&mut self, out: &mut Output) {
        if !self.ready() || self.inboxing.view.is_some() {
            return;
        }
        self.inboxing.view = Some(InboxView {
            entries: Vec::new(),
            unread_only: false,
            status: InboxStatus::Idle,
            has_more: false,
            marking_read: false,
            generation: 0,
        });
        self.start_inbox_request(true, false, out);
    }

    fn load_inbox(&mut self, unread_only: bool, out: &mut Output) {
        if !self.ready() {
            return;
        }
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        view.unread_only = unread_only;
        self.start_inbox_request(true, false, out);
    }

    fn load_more_inbox(&mut self, out: &mut Output) {
        let Some(view) = self.inboxing.view.as_ref() else {
            return;
        };
        if !self.ready()
            || !view.has_more
            || view.status == InboxStatus::Loading
            || view.entries.is_empty()
        {
            return;
        }
        self.start_inbox_request(false, false, out);
    }

    pub(super) fn close_inbox(&mut self, out: &mut Output) {
        if self.inboxing.view.take().is_none() {
            return;
        }
        if self.inboxing.in_flight.take().is_some() {
            out.effects.push(Effect::CancelInbox);
        }
        self.inboxing.stale = false;
        self.inboxing.read_rounds = 0;
        out.changes.push(Change::InboxChanged);
    }

    /// Forgets the inbox (sign-out).
    pub(super) fn clear_inbox(&mut self, out: &mut Output) {
        self.close_inbox(out);
    }

    /// Asks for a page. A `quiet` request replaces the entries when it is answered without
    /// clearing them or changing the status meanwhile.
    fn start_inbox_request(&mut self, first: bool, quiet: bool, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        let before = if first {
            None
        } else {
            view.entries
                .last()
                .and_then(|entry| entry.root.thread.as_ref())
                .map(|thread| thread.last_reply_seq)
        };
        self.inboxing.last_generation += 1;
        let generation = self.inboxing.last_generation;
        view.generation = generation;
        if !quiet {
            view.status = InboxStatus::Loading;
            if first {
                view.entries.clear();
                view.has_more = false;
            }
        }
        let query = InboxQuery {
            unread: view.unread_only.then_some(true),
            before,
            limit: Some(INBOX_PAGE),
        };
        self.inboxing.in_flight = Some(Request { generation, first });
        self.inboxing.stale = false;
        out.effects.push(Effect::LoadInbox {
            workspace,
            generation,
            query,
        });
        out.changes.push(Change::InboxChanged);
    }

    fn inbox_loaded(
        &mut self,
        generation: u64,
        result: Result<InboxPage, RequestError>,
        out: &mut Output,
    ) {
        if self
            .inboxing
            .in_flight
            .as_ref()
            .is_none_or(|request| request.generation != generation)
            || self.inboxing.view.is_none()
        {
            return;
        }
        let Some(request) = self.inboxing.in_flight.take() else {
            return;
        };
        match result {
            Ok(page) => {
                if let Some(view) = self.inboxing.view.as_mut() {
                    if request.first {
                        view.entries = page.entries;
                    } else {
                        for entry in page.entries {
                            if !view
                                .entries
                                .iter()
                                .any(|held| held.root.id == entry.root.id)
                            {
                                view.entries.push(entry);
                            }
                        }
                    }
                    view.has_more = page.has_more;
                    view.status = InboxStatus::Ready;
                }
                // What happened to the threads while the page was on its way may not be in it.
                if self.inboxing.stale {
                    self.start_inbox_request(true, true, out);
                }
            }
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(error) => {
                let (reason, can_retry, gone) = inbox_failure(&error);
                if gone {
                    self.set_notice(Some(reason.clone()), out);
                }
                if let Some(view) = self.inboxing.view.as_mut() {
                    view.status = InboxStatus::Failed { reason, can_retry };
                }
            }
        }
        out.changes.push(Change::InboxChanged);
    }

    fn mark_inbox_read(&mut self, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        if !self.ready() {
            return;
        }
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        if view.marking_read {
            return;
        }
        view.marking_read = true;
        // Shown read at once; the events that follow say the same.
        for entry in &mut view.entries {
            entry.state.unread_count = 0;
            entry.state.mention_count = 0;
        }
        self.inboxing.read_rounds = 1;
        out.effects.push(Effect::MarkInboxRead { workspace });
        out.changes.push(Change::InboxChanged);
    }

    fn inbox_read_result(
        &mut self,
        result: Result<lazy_protocol::InboxReadResponse, RequestError>,
        out: &mut Output,
    ) {
        if !self
            .inboxing
            .view
            .as_ref()
            .is_some_and(|view| view.marking_read)
        {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            // The person lost the workspace meanwhile: there is nothing to ask again, and the
            // rows must follow the thread states again.
            if let Some(view) = self.inboxing.view.as_mut() {
                view.marking_read = false;
            }
            return;
        };
        match result {
            Ok(response) => {
                // The server marks a bounded number at a time: ask again until none are left.
                if response.updated >= INBOX_READ_CAP && self.inboxing.read_rounds < MAX_READ_ROUNDS
                {
                    self.inboxing.read_rounds += 1;
                    out.effects.push(Effect::MarkInboxRead { workspace });
                    return;
                }
                if let Some(view) = self.inboxing.view.as_mut() {
                    view.marking_read = false;
                }
            }
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(error) => {
                if let Some(view) = self.inboxing.view.as_mut() {
                    view.marking_read = false;
                }
                let (reason, _, _) = inbox_failure(&error);
                self.set_notice(
                    Some(format!("Couldn't mark the threads read. {reason}")),
                    out,
                );
                // Show what the server holds.
                self.inbox_refresh(out);
            }
        }
        out.changes.push(Change::InboxChanged);
    }

    /// An entry shows the person's state for its thread as the core holds it, which counts new
    /// replies as they arrive (the server announces a thread's state only when it is followed,
    /// unfollowed, or read). While "mark all read" is out the entries stay as they were made.
    pub(super) fn sync_inbox_states(&mut self, out: &mut Output) {
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        if view.marking_read {
            return;
        }
        let mut changed = false;
        for entry in &mut view.entries {
            if let Some(state) = self.thread_states.get(&entry.root.id)
                && *state != entry.state
            {
                entry.state = state.clone();
                changed = true;
            }
        }
        if changed {
            out.changes.push(Change::InboxChanged);
        }
    }

    // ----- Events -----

    /// The log was skipped (a snapshot replaced it): threads may have become followed, active, or
    /// gone without an event, so what is shown may be out of date. Ask again.
    pub(super) fn inbox_resynced(&mut self, out: &mut Output) {
        self.inbox_refresh(out);
    }

    /// Asks for the first page again without disturbing what is shown.
    fn inbox_refresh(&mut self, out: &mut Output) {
        let Some(view) = self.inboxing.view.as_ref() else {
            return;
        };
        if view.status == InboxStatus::Idle {
            return;
        }
        if self.inboxing.in_flight.is_some() {
            self.inboxing.stale = true;
        } else {
            self.start_inbox_request(true, true, out);
        }
    }

    pub(super) fn inbox_thread_state(&mut self, state: &ThreadUserState, out: &mut Output) {
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        self.inboxing.stale |= self.inboxing.in_flight.is_some();
        let root = state.root_id;
        match view.entries.iter().position(|entry| entry.root.id == root) {
            Some(index) if !state.following => {
                view.entries.remove(index);
                out.changes.push(Change::InboxChanged);
            }
            Some(index) => {
                if view.entries[index].state != *state {
                    view.entries[index].state = state.clone();
                    out.changes.push(Change::InboxChanged);
                }
            }
            None if state.following && (!view.unread_only || state.unread_count > 0) => {
                self.inbox_refresh(out);
            }
            None => {}
        }
    }

    pub(super) fn inbox_thread_changed(
        &mut self,
        root: MessageId,
        summary: Option<&ThreadSummary>,
        out: &mut Output,
    ) {
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        self.inboxing.stale |= self.inboxing.in_flight.is_some();
        match view.entries.iter().position(|entry| entry.root.id == root) {
            Some(index) => {
                match summary {
                    None => {
                        view.entries.remove(index);
                    }
                    Some(summary) => {
                        let newer = view.entries[index]
                            .root
                            .thread
                            .as_ref()
                            .is_none_or(|held| summary.as_of >= held.as_of);
                        if newer {
                            view.entries[index].root.thread = Some(summary.clone());
                            view.entries
                                .sort_by_key(|entry| std::cmp::Reverse(activity(entry)));
                        }
                    }
                }
                out.changes.push(Change::InboxChanged);
            }
            None => {
                let followed = self
                    .thread_states
                    .get(&root)
                    .is_some_and(|state| state.following);
                if summary.is_some() && followed {
                    self.inbox_refresh(out);
                }
            }
        }
    }

    /// A reply arrived: it is the newest one of its thread, if no newer is shown.
    pub(super) fn inbox_message_created(&mut self, message: &Message, out: &mut Output) {
        let Some(root) = message.thread_root else {
            return;
        };
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        self.inboxing.stale |= self.inboxing.in_flight.is_some();
        if let Some(entry) = view.entries.iter_mut().find(|entry| entry.root.id == root)
            && entry
                .last_reply
                .as_ref()
                .is_none_or(|held| message.seq > held.seq)
        {
            entry.last_reply = Some(message.clone());
            out.changes.push(Change::InboxChanged);
        }
    }

    pub(super) fn inbox_message_edited(&mut self, message: &Message, out: &mut Output) {
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        self.inboxing.stale |= self.inboxing.in_flight.is_some();
        let mut changed = false;
        for entry in &mut view.entries {
            for held in [Some(&mut entry.root), entry.last_reply.as_mut()]
                .into_iter()
                .flatten()
            {
                if held.id == message.id && held.revision < message.revision && !message.deleted {
                    *held = message.clone();
                    changed = true;
                }
            }
        }
        if changed {
            out.changes.push(Change::InboxChanged);
        }
    }

    pub(super) fn inbox_message_deleted(&mut self, message: &Message, out: &mut Output) {
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        self.inboxing.stale |= self.inboxing.in_flight.is_some();
        let mut changed = false;
        for entry in &mut view.entries {
            if entry.root.id == message.id {
                entry.root = message.clone();
                changed = true;
            }
            if entry
                .last_reply
                .as_ref()
                .is_some_and(|reply| reply.id == message.id)
            {
                entry.last_reply = None;
                changed = true;
            }
        }
        if changed {
            out.changes.push(Change::InboxChanged);
        }
    }

    pub(super) fn inbox_conversation_gone(
        &mut self,
        conversation: ConversationId,
        out: &mut Output,
    ) {
        let Some(view) = self.inboxing.view.as_mut() else {
            return;
        };
        let before = view.entries.len();
        view.entries
            .retain(|entry| entry.conversation.id != conversation);
        if view.entries.len() != before {
            out.changes.push(Change::InboxChanged);
        }
    }
}

/// The position of an entry's newest reply, for ordering.
fn activity(entry: &InboxEntry) -> EventSeq {
    entry
        .root
        .thread
        .as_ref()
        .map_or(EventSeq::ZERO, |thread| thread.last_reply_seq)
}

/// Words for a person about why the inbox failed, whether asking again can help, and whether the
/// workspace is no longer theirs.
fn inbox_failure(error: &RequestError) -> (String, bool, bool) {
    match error {
        RequestError::Unknown(_) => ("Couldn't reach the server.".to_owned(), true, false),
        RequestError::Rejected(api) => match api.code {
            ErrorCode::RateLimited => (
                "Too many requests. Wait a moment and try again.".to_owned(),
                true,
                false,
            ),
            ErrorCode::Forbidden | ErrorCode::NotFound => (
                "You can't open threads in this workspace any more.".to_owned(),
                false,
                true,
            ),
            ErrorCode::InvalidRequest => (api.message.clone(), false, false),
            _ => (
                "Something went wrong with the threads.".to_owned(),
                true,
                false,
            ),
        },
    }
}
