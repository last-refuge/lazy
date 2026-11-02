//! Threads (D15): the pane, its replies, following, and per-thread read state.
//!
//! # Shape
//!
//! A thread is a root (a top-level message) and its replies. The channel's timeline lists roots
//! (with their [`ThreadSummary`](lazy_protocol::ThreadSummary)) and the replies that were also
//! sent to the channel; every other reply lives only in its thread. One thread pane is open at a
//! time, and only the open thread's replies are held:
//!
//! * `pane.root` is a one-row timeline for the root, seeded from the channel's row (or fetched),
//!   so the root takes the same edits, reactions, and summaries as everywhere else;
//! * `pane.replies` is a [`Timeline`] of the replies, paged like history.
//!
//! A message can therefore be in up to two timelines at once (an also-in-channel reply is in the
//! channel's and the pane's; a root is in the channel's and the pane's). Everything that updates
//! a message goes through [`ClientCore::homes_of`], so each copy stays current.
//!
//! Replies sent from this client are operations like sends (journaled, reconciled through the
//! operation status). Their pending rows live in the replies timeline; when the pane is closed
//! the unconfirmed rows are parked, so a failed reply keeps its text and a confirmation still
//! finds its row.

use lazy_domain::{ChannelAccess, MessageFacts};
use lazy_protocol::{Attachment, ThreadUserState};

use super::*;
use crate::attachments::UploadProgress;
use crate::model::{MessageExcerpt, OpenThread, ThreadDraft, ThreadView};
use crate::timeline::SummaryOutcome;

/// Longest excerpt of a message offered by [`ClientCore::message_excerpt`], in characters.
const EXCERPT_CHARS: usize = 160;

/// What a reply sent from here asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReplyTo {
    pub(super) root: MessageId,
    pub(super) also_in_channel: bool,
}

/// The open thread pane.
#[derive(Debug)]
pub(super) struct Pane {
    pub(super) conversation: ConversationId,
    pub(super) root: MessageId,
    /// The root message: a timeline of at most one row, so it is updated like any other message.
    pub(super) root_timeline: Timeline,
    /// The replies, oldest first, then the ones still being sent.
    pub(super) replies: Timeline,
    /// How many fetches of the root are in flight. A new snapshot asks again while an earlier
    /// answer may still be on its way, and answers are not ordered: each is merged (the newer
    /// summary wins) and only the last one to arrive settles the pane.
    pub(super) root_fetches: u32,
}

/// Where a copy of a message can live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Home {
    /// The conversation's timeline.
    Channel(ConversationId),
    /// The open thread's root row.
    PaneRoot,
    /// The open thread's replies.
    PaneReplies,
}

/// Rewrites the changes a timeline produced for a thread pane into the thread's own changes.
/// `conversation` is the one the timeline was told it belongs to.
pub(super) fn pane_changes(root: MessageId, replies: bool, changes: Vec<Change>) -> Vec<Change> {
    let mut mapped = Vec::new();
    for change in changes {
        let next = match change {
            Change::TimelineSplice {
                at,
                removed,
                inserted,
                ..
            } if replies => Change::ThreadSplice {
                root,
                at,
                removed,
                inserted,
            },
            Change::TimelineItemUpdated { index, .. } if replies => {
                Change::ThreadItemUpdated { root, index }
            }
            Change::TimelineReset { .. } if replies => Change::ThreadReset { root },
            Change::HistoryStatusChanged { .. } if replies => Change::ThreadHistoryChanged { root },
            Change::TimelineSplice { .. }
            | Change::TimelineItemUpdated { .. }
            | Change::TimelineReset { .. } => Change::ThreadRootChanged { root },
            other => other,
        };
        if !mapped.contains(&next) || matches!(next, Change::ThreadSplice { .. }) {
            mapped.push(next);
        }
    }
    mapped
}

// ----- Reading -----

impl ClientCore {
    /// The thread whose pane is open.
    pub fn open_thread(&self) -> Option<OpenThread> {
        self.pane.as_ref().map(|pane| OpenThread {
            conversation: pane.conversation,
            root: pane.root,
        })
    }

    /// What the pane of the open thread `root` shows; `None` when another thread (or none) is
    /// open.
    pub fn thread(&self, root: MessageId) -> Option<ThreadView<'_>> {
        let pane = self.pane.as_ref().filter(|pane| pane.root == root)?;
        Some(ThreadView {
            root: pane.root_timeline.items().first(),
            replies: pane.replies.items(),
            history: &pane.replies.history,
            state: self.thread_states.get(&root),
            draft: self.thread_draft(root),
        })
    }

    /// The person's following flag, read position, and counts for a thread, if the client holds
    /// them (the server lists the threads a person follows or has unread replies in).
    pub fn thread_state(&self, root: MessageId) -> Option<&ThreadUserState> {
        self.thread_states.get(&root)
    }

    /// Unread replies across the threads the person follows.
    pub fn followed_thread_unread_total(&self) -> u32 {
        self.thread_states
            .values()
            .filter(|state| state.following)
            .fold(0u32, |total, state| {
                total.saturating_add(state.unread_count)
            })
    }

    /// The reply composer text saved for a thread.
    pub fn thread_draft(&self, root: MessageId) -> &str {
        self.thread_drafts
            .get(&root)
            .map_or("", |(_, text)| text.as_str())
    }

    /// Whether to offer "Reply in thread" on a message: a top-level message that has not been
    /// deleted, in a conversation the person can post in. The server decides; this only avoids
    /// offering the impossible.
    pub fn can_reply_message(&self, conversation: ConversationId, message: MessageId) -> bool {
        let Some(row) = self.find_row(conversation, message) else {
            return false;
        };
        row.thread_root.is_none()
            && row.is_confirmed()
            && self
                .message_facts(conversation, message)
                .is_some_and(|facts| facts.can_reply())
    }

    /// The start of a message, for a header that points at it, answered from whatever the core
    /// holds (the channel's timeline or the open thread). `None` if it is not loaded.
    pub fn message_excerpt(
        &self,
        conversation: ConversationId,
        message: MessageId,
    ) -> Option<MessageExcerpt> {
        let row = self.find_row(conversation, message)?;
        let mut plain = row.document.plain_text();
        // A message that is only files reads as the names of its files.
        if plain.is_empty() {
            plain = match row.attachments.as_slice() {
                [] => String::new(),
                [only] => only.name.clone(),
                many => format!("{} files", many.len()),
            };
        }
        let text: String = plain.chars().take(EXCERPT_CHARS).collect();
        Some(MessageExcerpt {
            author: row.author_id,
            text,
        })
    }

    /// The row showing `message`, in the conversation's timeline or the open thread.
    pub(super) fn find_row(
        &self,
        conversation: ConversationId,
        message: MessageId,
    ) -> Option<&TimelineItem> {
        if let Some(row) = self
            .timelines
            .get(&conversation)
            .and_then(|timeline| timeline.message_row(message))
        {
            return Some(row);
        }
        let pane = self
            .pane
            .as_ref()
            .filter(|pane| pane.conversation == conversation)?;
        pane.root_timeline
            .message_row(message)
            .or_else(|| pane.replies.message_row(message))
    }

    pub(super) fn message_facts(
        &self,
        conversation: ConversationId,
        message: MessageId,
    ) -> Option<MessageFacts> {
        let info = self.conversation(conversation)?;
        let me = self.my_id()?;
        let row = self.find_row(conversation, message)?;
        Some(MessageFacts {
            access: ChannelAccess {
                workspace_role: self.role_in(info.workspace_id),
                is_channel_member: true,
                visibility: info.visibility,
                archived: info.archived,
            },
            is_author: row.author_id == me,
            deleted: row.deleted,
        })
    }
}

// ----- Where a message lives -----

impl ClientCore {
    /// The timelines that can hold a copy of a message of `conversation`: the conversation's own
    /// and, while a pane of that conversation is open, its root and replies. A timeline that
    /// does not hold the message still counts, because it may be about to load it.
    pub(super) fn homes_of(&self, conversation: ConversationId) -> Vec<Home> {
        let mut homes = Vec::new();
        if self.timelines.contains_key(&conversation) {
            homes.push(Home::Channel(conversation));
        }
        if self
            .pane
            .as_ref()
            .is_some_and(|pane| pane.conversation == conversation)
        {
            homes.push(Home::PaneRoot);
            homes.push(Home::PaneReplies);
        }
        homes
    }

    /// Runs `f` on one copy's timeline, then reports what changed in terms of that copy.
    pub(super) fn in_home<R>(
        &mut self,
        home: Home,
        conversation: ConversationId,
        out: &mut Output,
        f: impl FnOnce(&mut Timeline, &mut Vec<Change>) -> R,
    ) -> Option<R> {
        let mut changes = Vec::new();
        let result = match home {
            Home::Channel(id) => f(self.timelines.get_mut(&id)?, &mut changes),
            Home::PaneRoot => {
                let pane = self.pane.as_mut()?;
                let result = f(&mut pane.root_timeline, &mut changes);
                let root = pane.root;
                out.changes.extend(pane_changes(root, false, changes));
                return Some(result);
            }
            Home::PaneReplies => {
                let pane = self.pane.as_mut()?;
                let result = f(&mut pane.replies, &mut changes);
                let root = pane.root;
                out.changes.extend(pane_changes(root, true, changes));
                return Some(result);
            }
        };
        let _ = conversation;
        out.changes.extend(changes);
        Some(result)
    }

    /// Runs `f` on every timeline that can hold a copy of a message of `conversation`.
    pub(super) fn each_home(
        &mut self,
        conversation: ConversationId,
        out: &mut Output,
        mut f: impl FnMut(Home, &mut Timeline, &mut Vec<Change>),
    ) {
        for home in self.homes_of(conversation) {
            self.in_home(home, conversation, out, |timeline, changes| {
                f(home, timeline, changes)
            });
        }
    }

    /// The content the server has for a message held in any of its homes, not counting an
    /// unsaved edit.
    pub(super) fn saved_document_of(
        &self,
        conversation: ConversationId,
        message: MessageId,
    ) -> Option<&MessageDocument> {
        if let Some(timeline) = self.timelines.get(&conversation)
            && let Some(document) = timeline
                .index_of_message(message)
                .and_then(|_| timeline.saved_document(message))
        {
            return Some(document);
        }
        let pane = self
            .pane
            .as_ref()
            .filter(|pane| pane.conversation == conversation)?;
        pane.root_timeline
            .index_of_message(message)
            .and_then(|_| pane.root_timeline.saved_document(message))
            .or_else(|| {
                pane.replies
                    .index_of_message(message)
                    .and_then(|_| pane.replies.saved_document(message))
            })
    }

    /// The reactions the server has on a message, in whichever timeline holds it, not counting
    /// the person's unsaved changes.
    pub(super) fn server_reactions_any(
        &self,
        conversation: ConversationId,
        message: MessageId,
    ) -> Option<&[lazy_protocol::ReactionSummary]> {
        if let Some(held) = self
            .timelines
            .get(&conversation)
            .and_then(|timeline| timeline.server_reactions(message))
        {
            return Some(held);
        }
        let pane = self
            .pane
            .as_ref()
            .filter(|pane| pane.conversation == conversation)?;
        pane.root_timeline
            .server_reactions(message)
            .or_else(|| pane.replies.server_reactions(message))
    }

    /// Whether the server has `user` reacting with `emoji` on a message, in whichever timeline
    /// holds it, not counting the person's unsaved changes.
    pub(super) fn server_reacted_any(
        &self,
        conversation: ConversationId,
        message: MessageId,
        user: UserId,
        emoji: ReactionEmoji,
    ) -> Option<bool> {
        if let Some(held) = self
            .timelines
            .get(&conversation)
            .and_then(|timeline| timeline.server_reacted(message, user, emoji))
        {
            return Some(held);
        }
        let pane = self
            .pane
            .as_ref()
            .filter(|pane| pane.conversation == conversation)?;
        pane.root_timeline
            .server_reacted(message, user, emoji)
            .or_else(|| pane.replies.server_reacted(message, user, emoji))
    }
}

// ----- Opening and closing -----

impl ClientCore {
    pub(super) fn open_thread_input(
        &mut self,
        conversation: ConversationId,
        root: MessageId,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) || self.conversation(conversation).is_none() {
            return;
        }
        if self
            .pane
            .as_ref()
            .is_some_and(|pane| pane.root == root && pane.conversation == conversation)
        {
            return;
        }
        if self.selected != Some(conversation) {
            self.select_conversation(conversation, out);
        }
        self.close_pane(out);
        let mut pane = Pane {
            conversation,
            root,
            root_timeline: Timeline::new(),
            replies: self
                .parked_replies
                .remove(&root)
                .unwrap_or_else(Timeline::new),
            root_fetches: 0,
        };
        // The channel usually has the root already.
        let held = self
            .timelines
            .get(&conversation)
            .and_then(|timeline| timeline.message_of(root, conversation));
        let mut ignored = Vec::new();
        match held {
            Some(message) => {
                pane.root_timeline
                    .merge_messages(conversation, &[message], &mut ignored);
                pane.root_timeline.note_page(None, false);
            }
            None => {
                // Changes to the root that arrive before the answer must not be undone by it.
                pane.root_fetches += 1;
                pane.root_timeline.history = HistoryStatus::Loading;
                out.effects.push(Effect::FetchMessage { message: root });
            }
        }
        self.pane = Some(pane);
        out.changes.push(Change::OpenThreadChanged);
        self.request_replies(None, out);
    }

    pub(super) fn close_thread_input(&mut self, out: &mut Output) {
        self.close_pane(out);
    }

    /// The server asked for a fresh snapshot: what the pane holds is stale. Its confirmed rows
    /// go, and `refresh_pane` loads them again once the bootstrap has been applied.
    pub(super) fn reset_pane(&mut self, out: &mut Output) {
        let Some(conversation) = self.pane.as_ref().map(|pane| pane.conversation) else {
            return;
        };
        self.each_home(conversation, out, |home, timeline, changes| {
            if home != Home::Channel(conversation) {
                timeline.reset_keeping_local(conversation, changes);
            }
        });
    }

    /// Loads the open thread again after a new snapshot.
    pub(super) fn refresh_pane(&mut self, out: &mut Output) {
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        let root = pane.root;
        pane.root_fetches += 1;
        out.effects.push(Effect::FetchMessage { message: root });
        self.reset_pane(out);
        if let Some(pane) = self.pane.as_mut() {
            pane.root_timeline.history = HistoryStatus::Loading;
        }
        self.request_replies(None, out);
    }

    /// Closes the pane, if one is open. Rows that are still being sent are parked.
    pub(super) fn close_pane(&mut self, out: &mut Output) {
        let Some(pane) = self.pane.take() else {
            return;
        };
        let root = pane.root;
        let mut replies = pane.replies;
        self.thread_at_bottom.remove(&root);
        self.thread_mark_failed.remove(&root);
        let mut ignored = Vec::new();
        replies.reset_keeping_local(pane.conversation, &mut ignored);
        if replies.items().is_empty() {
            self.parked_replies.remove(&root);
        } else {
            self.parked_replies.insert(root, replies);
        }
        out.changes.push(Change::OpenThreadChanged);
    }

    fn request_replies(&mut self, before: Option<EventSeq>, out: &mut Output) {
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        let root = pane.root;
        self.next_replies_fetch += 1;
        pane.replies.generation = self.next_replies_fetch;
        pane.replies.clear_unloaded_updates();
        pane.replies.history = HistoryStatus::Loading;
        out.effects.push(Effect::FetchReplies {
            root,
            before,
            limit: Some(HISTORY_PAGE_LIMIT),
            generation: pane.replies.generation,
        });
        out.changes.push(Change::ThreadHistoryChanged { root });
    }

    pub(super) fn load_older_replies(&mut self, root: MessageId, out: &mut Output) {
        let Some(pane) = self.pane.as_ref().filter(|pane| pane.root == root) else {
            return;
        };
        match pane.replies.history {
            HistoryStatus::Ready { has_more: true } | HistoryStatus::Failed { .. } => {
                let before = pane.replies.older_cursor();
                self.request_replies(before, out);
            }
            HistoryStatus::NotLoaded => self.request_replies(None, out),
            HistoryStatus::Ready { has_more: false } | HistoryStatus::Loading => {}
        }
    }

    pub(super) fn replies_loaded(
        &mut self,
        root: MessageId,
        generation: u64,
        result: Result<lazy_protocol::HistoryPage, RequestError>,
        out: &mut Output,
    ) {
        let Some(pane) = self.pane.as_mut().filter(|pane| pane.root == root) else {
            return;
        };
        if pane.replies.generation != generation || pane.replies.history != HistoryStatus::Loading {
            return;
        }
        let conversation = pane.conversation;
        match result {
            Ok(page) => {
                let belongs: Vec<Message> = page
                    .messages
                    .into_iter()
                    .filter(|m| m.conversation_id == conversation && m.thread_root == Some(root))
                    .collect();
                let mut changes = Vec::new();
                pane.replies
                    .merge_messages(conversation, &belongs, &mut changes);
                pane.replies.clear_unloaded_updates();
                pane.replies
                    .note_page(belongs.iter().map(|m| m.seq).min(), page.has_more);
                pane.replies.history = HistoryStatus::Ready {
                    has_more: page.has_more,
                };
                changes.push(Change::HistoryStatusChanged { conversation });
                out.changes.extend(pane_changes(root, true, changes));
                self.resync_pane_rows(out);
            }
            Err(error) => {
                pane.replies.history = HistoryStatus::Failed {
                    message: error.message(),
                };
                out.changes.push(Change::ThreadHistoryChanged { root });
                if error.is_unauthenticated() {
                    self.session_expired(out);
                } else if matches!(&error, RequestError::Rejected(e) if e.code == lazy_protocol::ErrorCode::NotFound)
                {
                    self.thread_gone(root, out);
                }
            }
        }
        self.permalink_advance(out);
    }

    /// The answer to fetching the open thread's root (the channel did not have it loaded).
    pub(super) fn pane_root_fetched(
        &mut self,
        message: MessageId,
        result: &Result<lazy_protocol::MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(pane) = self
            .pane
            .as_mut()
            .filter(|pane| pane.root == message && pane.root_fetches > 0)
        else {
            return;
        };
        // Answers to fetches issued before and after a snapshot can arrive in either order: each
        // one is merged, the newer summary wins, and only the last one settles a failure.
        pane.root_fetches -= 1;
        let (root, conversation, last) = (pane.root, pane.conversation, pane.root_fetches == 0);
        match result {
            Ok(response) => {
                let fetched = &response.message;
                // Only a top-level message of this conversation has a thread; a deleted one
                // without replies has nothing left to show.
                if fetched.conversation_id != conversation
                    || fetched.thread_root.is_some()
                    || (fetched.deleted && fetched.thread.is_none())
                {
                    return self.thread_gone(root, out);
                }
                self.in_home(Home::PaneRoot, conversation, out, |timeline, changes| {
                    timeline.merge_messages(conversation, std::slice::from_ref(fetched), changes);
                    timeline.clear_unloaded_updates();
                    timeline.note_page(None, false);
                    timeline.history = HistoryStatus::Ready { has_more: false };
                });
                self.resync_pane_rows(out);
                self.permalink_advance(out);
            }
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(RequestError::Rejected(error))
                if matches!(
                    error.code,
                    lazy_protocol::ErrorCode::NotFound | lazy_protocol::ErrorCode::Forbidden
                ) =>
            {
                self.thread_gone(root, out)
            }
            Err(_) if !last => {}
            Err(_)
                if self
                    .pane
                    .as_ref()
                    .is_some_and(|pane| !pane.root_timeline.items().is_empty()) => {}
            Err(error) => {
                if let Some(pane) = self.pane.as_mut() {
                    pane.root_timeline.history = HistoryStatus::NotLoaded;
                }
                let notice = format!(
                    "Couldn't load the message that started this thread. {}",
                    super::conversations::readable_error(error)
                );
                self.set_notice(Some(notice), out);
            }
        }
    }

    /// The connection is back: the open thread's root is fetched again if the last attempt
    /// failed and the channel does not have it.
    pub(super) fn retry_pane_root(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        if pane.root_fetches > 0 || !pane.root_timeline.items().is_empty() {
            return;
        }
        pane.root_fetches += 1;
        pane.root_timeline.history = HistoryStatus::Loading;
        out.effects
            .push(Effect::FetchMessage { message: pane.root });
    }

    /// The thread can no longer be read: say so and close it.
    pub(super) fn thread_gone(&mut self, root: MessageId, out: &mut Output) {
        if self.pane.as_ref().is_some_and(|pane| pane.root == root) {
            self.close_pane(out);
            let notice = "That thread is no longer available.";
            self.set_notice(Some(notice.to_owned()), out);
        }
    }

    /// Re-applies edit states and pending reactions to rows of the pane that just appeared.
    pub(super) fn resync_pane_rows(&mut self, out: &mut Output) {
        let Some(conversation) = self.pane.as_ref().map(|pane| pane.conversation) else {
            return;
        };
        self.resync_edit_rows(conversation, out);
        self.resync_reaction_rows(conversation, out);
    }
}

// ----- Drafts -----

impl ClientCore {
    pub(super) fn set_thread_draft(&mut self, root: MessageId, text: String, out: &mut Output) {
        if self.thread_draft(root) == text {
            return;
        }
        let Some(conversation) = self.thread_conversation(root) else {
            return;
        };
        if text.is_empty() {
            self.thread_drafts.remove(&root);
            self.thread_selections.remove(&root);
        } else {
            self.thread_drafts.insert(root, (conversation, text));
        }
        out.changes.push(Change::ThreadDraftChanged { root });
        self.persist_thread_drafts(out);
    }

    pub(super) fn persist_thread_drafts(&self, out: &mut Output) {
        let mut drafts: Vec<ThreadDraft> = self
            .thread_drafts
            .iter()
            .map(|(root, (conversation, text))| ThreadDraft {
                root: *root,
                conversation: *conversation,
                text: text.clone(),
                selection: self
                    .thread_selections
                    .get(root)
                    .map(|held| super::view::clamp_selection(text, *held)),
            })
            .collect();
        drafts.sort_by_key(|draft| draft.root);
        out.effects.push(Effect::PersistThreadDrafts(drafts));
    }

    /// The conversation a thread belongs to, as far as the client knows: the open pane, a held
    /// state, a saved draft, or a root row in a timeline.
    pub(super) fn thread_conversation(&self, root: MessageId) -> Option<ConversationId> {
        if let Some(pane) = self.pane.as_ref().filter(|pane| pane.root == root) {
            return Some(pane.conversation);
        }
        if let Some(state) = self.thread_states.get(&root) {
            return Some(state.conversation_id);
        }
        if let Some((conversation, _)) = self.thread_drafts.get(&root) {
            return Some(*conversation);
        }
        self.timelines
            .iter()
            .find(|(_, timeline)| timeline.index_of_message(root).is_some())
            .map(|(conversation, _)| *conversation)
    }
}

// ----- Rows being sent -----

/// The row for a message this client is sending.
pub(super) fn local_row(
    operation: OperationId,
    author: UserId,
    document: MessageDocument,
    delivery: Delivery,
    thread: Option<ReplyTo>,
    attachments: Vec<Attachment>,
    upload: Option<UploadProgress>,
) -> TimelineItem {
    TimelineItem {
        key: ItemKey::Operation(operation),
        author_id: author,
        document,
        delivery,
        edited_at: None,
        edit: EditState::None,
        reactions: Vec::new(),
        reactions_as_of: EventSeq::ZERO,
        rich: None,
        mentions: Mentions::default(),
        thread: None,
        thread_root: thread.map(|reply| reply.root),
        in_channel: thread.is_some_and(|reply| reply.also_in_channel),
        deleted: false,
        attachments,
        upload,
    }
}

impl ClientCore {
    /// Appends the row of a message being sent: to the conversation's timeline, or for a reply to
    /// its thread's (the open pane, or parked rows while it is closed).
    pub(super) fn push_local_row(
        &mut self,
        conversation: ConversationId,
        thread: Option<ReplyTo>,
        row: TimelineItem,
        out: &mut Output,
    ) {
        if let Some(reply) = thread
            && self
                .pane
                .as_ref()
                .is_none_or(|pane| pane.root != reply.root)
        {
            self.parked_replies
                .entry(reply.root)
                .or_insert_with(Timeline::new)
                .push_local(conversation, row, &mut Vec::new());
            return;
        }
        self.in_local_timeline(conversation, thread, out, |timeline, changes| {
            timeline.push_local(conversation, row, changes)
        });
    }

    /// Runs `f` on the timeline that holds the rows of messages being sent for `thread` (the
    /// conversation's own when `None`). Changes are reported only for a timeline on screen.
    pub(super) fn in_local_timeline<R>(
        &mut self,
        conversation: ConversationId,
        thread: Option<ReplyTo>,
        out: &mut Output,
        f: impl FnOnce(&mut Timeline, &mut Vec<Change>) -> R,
    ) -> Option<R> {
        let mut changes = Vec::new();
        let Some(reply) = thread else {
            let result = f(self.timeline_mut(conversation), &mut changes);
            out.changes.extend(changes);
            return Some(result);
        };
        if let Some(pane) = self.pane.as_mut().filter(|pane| pane.root == reply.root) {
            let result = f(&mut pane.replies, &mut changes);
            out.changes.extend(pane_changes(reply.root, true, changes));
            return Some(result);
        }
        let parked = self.parked_replies.get_mut(&reply.root)?;
        Some(f(parked, &mut changes))
    }

    /// The row of an operation that has not been confirmed, with the thread it goes to.
    pub(super) fn locate_local_row(
        &self,
        conversation: ConversationId,
        operation: OperationId,
    ) -> Option<(TimelineItem, Option<ReplyTo>)> {
        let key = ItemKey::Operation(operation);
        let timelines = self
            .timelines
            .get(&conversation)
            .into_iter()
            .chain(
                self.pane
                    .as_ref()
                    .filter(|pane| pane.conversation == conversation)
                    .map(|pane| &pane.replies),
            )
            .chain(self.parked_replies.values());
        for timeline in timelines {
            if let Some(index) = timeline.index_of(key) {
                let item = timeline.items()[index].clone();
                let thread = item.thread_root.map(|root| ReplyTo {
                    root,
                    also_in_channel: item.in_channel,
                });
                return Some((item, thread));
            }
        }
        None
    }

    /// A parked thread keeps only rows that are still being sent.
    pub(super) fn tidy_parked(&mut self, thread: Option<ReplyTo>) {
        let Some(reply) = thread else {
            return;
        };
        if self
            .pane
            .as_ref()
            .is_some_and(|pane| pane.root == reply.root)
        {
            return;
        }
        if let Some(parked) = self.parked_replies.get_mut(&reply.root) {
            parked.reset_keeping_local(ConversationId::generate(), &mut Vec::new());
            if parked.items().is_empty() {
                self.parked_replies.remove(&reply.root);
            }
        }
    }

    /// Puts a confirmed message into the timelines that list it: a reply into the open thread's
    /// replies, and into the conversation's timeline when it was also sent there; any other
    /// message into the conversation's. Returns how many rows were added.
    pub(super) fn place_message(&mut self, message: &Message, out: &mut Output) -> usize {
        let conversation = message.conversation_id;
        let mut added = 0;
        if let Some(root) = message.thread_root
            && self.pane.as_ref().is_some_and(|pane| pane.root == root)
        {
            added += self
                .in_home(Home::PaneReplies, conversation, out, |timeline, changes| {
                    timeline.merge_messages(conversation, std::slice::from_ref(message), changes)
                })
                .unwrap_or(0);
        }
        if message.thread_root.is_none() || message.in_channel {
            self.timeline_mut(conversation);
            added += self
                .in_home(
                    Home::Channel(conversation),
                    conversation,
                    out,
                    |timeline, changes| {
                        timeline.merge_messages(
                            conversation,
                            std::slice::from_ref(message),
                            changes,
                        )
                    },
                )
                .unwrap_or(0);
        }
        added
    }

    /// Alias kept for the confirmation of a reply.
    pub(super) fn place_reply(&mut self, message: &Message, out: &mut Output) {
        self.place_message(message, out);
    }
}

// ----- Replying -----

impl ClientCore {
    pub(super) fn send_reply(
        &mut self,
        conversation: ConversationId,
        root: MessageId,
        text: &str,
        also_in_channel: bool,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) || self.conversation(conversation).is_none() {
            return;
        }
        // A reply to a reply goes to the same root.
        let root = self
            .find_row(conversation, root)
            .and_then(|row| row.thread_root)
            .unwrap_or(root);
        if let Some(facts) = self.message_facts(conversation, root)
            && !facts.can_reply()
        {
            let notice = if facts.deleted {
                "That message was deleted, so it can't be replied to."
            } else {
                "This conversation is read-only, so you can't reply here."
            };
            self.set_notice(Some(notice.to_owned()), out);
            return;
        }
        let target = ComposerTarget::Thread(root);
        let document = match parse_composer(text, &self.mention_directory(conversation)) {
            Ok(document) => document,
            // A reply that is only files has no text.
            Err(DocumentError::Empty) if self.composer_allows_empty(target) => {
                MessageDocument::empty()
            }
            Err(DocumentError::Empty) => return,
            Err(error) => {
                self.set_notice(Some(format!("Reply not sent: {error}.")), out);
                return;
            }
        };
        let Some(files) = self.take_composer_files(target, out) else {
            return;
        };
        let reply = ReplyTo {
            root,
            also_in_channel,
        };
        self.submit(conversation, document, Some(reply), files, out);
        if self.thread_drafts.remove(&root).is_some() {
            out.changes.push(Change::ThreadDraftChanged { root });
            self.persist_thread_drafts(out);
        }
    }
}

// ----- Following and read state -----

/// A change to whether the person follows a thread that is not saved yet.
#[derive(Debug)]
pub(super) struct Follow {
    conversation: ConversationId,
    /// What the person wants now.
    desired: bool,
    /// What the server last said (a response or an event): what the display returns to if the
    /// change is given up, and what the cache records.
    server: bool,
    in_flight: Option<FollowInFlight>,
}

#[derive(Debug)]
struct FollowInFlight {
    /// What the request in flight asks for.
    following: bool,
    /// Repeats after an unknown outcome so far.
    repeats: u32,
}

impl ClientCore {
    /// Forgets everything held about threads (the session ended).
    pub(super) fn clear_threads(&mut self) {
        self.pane = None;
        self.parked_replies.clear();
        self.thread_states.clear();
        self.thread_as_of.clear();
        self.thread_counted.clear();
        self.thread_rebuilt.clear();
        self.thread_at_bottom.clear();
        self.thread_mark_in_flight.clear();
        self.thread_mark_failed.clear();
        self.follows.clear();
    }

    /// Forgets the threads of a conversation the person no longer belongs to.
    pub(super) fn forget_threads_of(&mut self, conversation: ConversationId, out: &mut Output) {
        if self
            .pane
            .as_ref()
            .is_some_and(|pane| pane.conversation == conversation)
        {
            self.close_pane(out);
        }
        let roots: Vec<MessageId> = self
            .thread_states
            .values()
            .filter(|state| state.conversation_id == conversation)
            .map(|state| state.root_id)
            .chain(
                self.thread_drafts
                    .iter()
                    .filter(|(_, (c, _))| *c == conversation)
                    .map(|(root, _)| *root),
            )
            .chain(
                self.follows
                    .iter()
                    .filter(|(_, follow)| follow.conversation == conversation)
                    .map(|(root, _)| *root),
            )
            .collect();
        for root in roots {
            self.forget_thread(root);
        }
        self.forget_conversation_files(conversation, out);
        // Rows still being sent to this conversation's threads can never be delivered.
        self.parked_replies.retain(|_, timeline| {
            timeline
                .items()
                .iter()
                .next()
                .is_none_or(|item| item.thread_root.is_none())
        });
        self.persist_thread_drafts(out);
    }

    /// Forgets a thread's state, tracking, and draft.
    pub(super) fn forget_thread(&mut self, root: MessageId) {
        self.thread_states.remove(&root);
        self.thread_as_of.remove(&root);
        self.thread_counted.remove(&root);
        self.thread_rebuilt.remove(&root);
        self.thread_at_bottom.remove(&root);
        self.thread_mark_in_flight.remove(&root);
        self.thread_mark_failed.remove(&root);
        self.follows.remove(&root);
        self.thread_drafts.remove(&root);
        self.thread_selections.remove(&root);
    }

    /// The conversation of a loaded message, whichever timeline holds it.
    pub(super) fn conversation_of_row(&self, message: MessageId) -> Option<ConversationId> {
        self.timelines
            .iter()
            .find(|(_, timeline)| timeline.index_of_message(message).is_some())
            .map(|(conversation, _)| *conversation)
            .or_else(|| {
                self.pane
                    .as_ref()
                    .filter(|pane| {
                        pane.root_timeline.index_of_message(message).is_some()
                            || pane.replies.index_of_message(message).is_some()
                    })
                    .map(|pane| pane.conversation)
            })
    }

    /// The connection is back: asks again about replies whose outcome is unknown and whose
    /// thread is not open, so nothing on screen can retry them. A thread can be gone by now (its
    /// first message deleted), and then the person could never see the row to retry it.
    pub(super) fn retry_unseen_replies(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let open = self.pane.as_ref().map(|pane| pane.root);
        let mut stalled: Vec<OperationId> = self
            .ops
            .iter()
            .filter(|(_, op)| {
                op.stage == OpStage::Failed
                    && op.thread.is_some_and(|reply| Some(reply.root) != open)
            })
            .map(|(operation, _)| *operation)
            .collect();
        stalled.sort();
        for operation in stalled {
            let order = self.take_order();
            let Some(op) = self.ops.get_mut(&operation) else {
                continue;
            };
            op.resends = 0;
            op.order = order;
            let (conversation, thread) = (op.conversation, op.thread);
            self.in_local_timeline(conversation, thread, out, |timeline, changes| {
                timeline.move_to_back(conversation, ItemKey::Operation(operation), changes);
            });
            self.begin_status_query(operation, out);
        }
    }

    pub(super) fn thread_at_bottom_changed(&mut self, root: MessageId, at_bottom: bool) {
        if at_bottom {
            self.thread_at_bottom.insert(root);
        } else {
            self.thread_at_bottom.remove(&root);
        }
        self.thread_mark_failed.remove(&root);
    }

    // ----- Events -----

    /// `ThreadChanged`: the root's new summary, whose version is the event's own position.
    pub(super) fn apply_thread_changed(
        &mut self,
        conversation: ConversationId,
        root: MessageId,
        summary: Option<lazy_protocol::ThreadSummary>,
        seq: EventSeq,
        out: &mut Output,
    ) {
        if self.conversation(conversation).is_none() {
            return;
        }
        let as_of = summary
            .as_ref()
            .map_or(seq, |summary| summary.as_of.max(seq));
        let mut changed = false;
        let mut root_removed = false;
        let mut thread_ended = false;
        self.each_home(conversation, out, |home, timeline, changes| {
            if home == Home::PaneReplies {
                return;
            }
            let outcome =
                timeline.apply_thread_summary(conversation, root, as_of, summary.clone(), changes);
            changed |= outcome != SummaryOutcome::Unchanged;
            thread_ended |= outcome == SummaryOutcome::RemovedRow;
            root_removed |= home == Home::PaneRoot && outcome == SummaryOutcome::RemovedRow;
        });
        if thread_ended {
            // The last reply of a deleted root went, and the server dropped everyone's state for
            // the thread with it.
            self.forget_thread(root);
            out.changes.push(Change::ThreadStateChanged { root });
        }
        if changed {
            self.cache_changed(false);
        }
        if root_removed {
            self.close_pane(out);
            let notice = "That thread has no messages left.";
            self.set_notice(Some(notice.to_owned()), out);
        }
    }

    /// `ThreadStateChanged`: the person's state for a thread, current as of the event.
    pub(super) fn apply_thread_state_changed(
        &mut self,
        mut state: ThreadUserState,
        seq: EventSeq,
        out: &mut Output,
    ) {
        if self.conversation(state.conversation_id).is_none() {
            return;
        }
        let root = state.root_id;
        // A follow change that is not saved yet stays on display over the server's flag.
        if let Some(follow) = self.follows.get_mut(&root) {
            follow.server = state.following;
            state.following = follow.desired;
        }
        self.thread_states.insert(root, state);
        self.thread_as_of.insert(root, seq);
        self.thread_counted.remove(&root);
        self.thread_rebuilt.remove(&root);
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    /// Takes the thread states out of a bootstrap. A fresh start replaces everything; a
    /// revalidation keeps the cached states (replay will bring them up to date) and only fills
    /// in threads the cache did not know, current to the bootstrap's `head`.
    pub(super) fn adopt_bootstrap_thread_states(
        &mut self,
        response: &BootstrapResponse,
        fresh: bool,
    ) {
        if fresh {
            self.thread_states.clear();
            self.thread_as_of.clear();
            self.thread_counted.clear();
            self.thread_rebuilt.clear();
            self.thread_mark_in_flight.clear();
            self.thread_mark_failed.clear();
        }
        for workspace in &response.workspaces {
            for state in &workspace.threads {
                let root = state.root_id;
                if self.conversation(state.conversation_id).is_some()
                    && !self.thread_states.contains_key(&root)
                {
                    let mut state = state.clone();
                    if let Some(follow) = self.follows.get_mut(&root) {
                        follow.server = state.following;
                        state.following = follow.desired;
                    }
                    self.thread_states.insert(root, state);
                    self.thread_as_of.insert(root, workspace.head);
                }
            }
        }
        let known = self.all_conversation_ids();
        self.thread_states
            .retain(|_, state| known.contains(&state.conversation_id));
        let held: HashSet<MessageId> = self.thread_states.keys().copied().collect();
        self.thread_as_of.retain(|root, _| held.contains(root));
        self.thread_counted.retain(|root, _| held.contains(root));
        self.thread_rebuilt.retain(|root, _| held.contains(root));
    }

    // ----- Counting -----

    /// The position a held thread state is current to, for counting. A state shown only for a
    /// follow change that is not saved yet has no position and holds no counts.
    fn counting_as_of(&self, root: MessageId) -> Option<EventSeq> {
        self.thread_as_of
            .get(&root)
            .copied()
            .filter(|as_of| *as_of != EventSeq::ZERO)
    }

    /// Whether a reply at `seq` was counted as unread: it has an entry, or the state counted it
    /// and the count has not been rebuilt from the rows since in a way that left it out.
    fn reply_was_counted(&self, root: MessageId, seq: EventSeq, as_of: EventSeq) -> bool {
        self.thread_counted
            .get(&root)
            .is_some_and(|counted| counted.contains_key(&seq))
            || (seq <= as_of
                && seq
                    > self
                        .thread_rebuilt
                        .get(&root)
                        .copied()
                        .unwrap_or(EventSeq::ZERO))
    }

    /// Applies a reply's creation to the thread's read state, with the discipline of the
    /// conversation's unread count (see `readstate`): events at or before the state's position
    /// are already in it, the person's own reply reads everything, and a tombstone from a
    /// replay never counts.
    pub(super) fn count_reply_event(&mut self, message: &Message, out: &mut Output) {
        let Some(root) = message.thread_root else {
            return;
        };
        let Some(as_of) = self.counting_as_of(root) else {
            return;
        };
        if message.seq <= as_of {
            return;
        }
        let own = self.my_id() == Some(message.author_id);
        if message.deleted && !own {
            return;
        }
        let mentions_me = self.my_id().is_some_and(|me| message.mentions.includes(me));
        let Some(state) = self.thread_states.get_mut(&root) else {
            return;
        };
        if own {
            state.last_read = state.last_read.max(message.seq);
            state.unread_count = 0;
            state.mention_count = 0;
            self.thread_counted.remove(&root);
        } else if message.seq > state.last_read {
            state.unread_count = state.unread_count.saturating_add(1);
            if mentions_me {
                state.mention_count = state.mention_count.saturating_add(1);
            }
            self.thread_counted
                .entry(root)
                .or_default()
                .insert(message.seq, mentions_me);
        } else {
            return;
        }
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    /// Applies a reply's deletion: a reply that was counted stops counting.
    pub(super) fn count_reply_deleted(
        &mut self,
        message: &Message,
        had_mentions: &Mentions,
        event_seq: EventSeq,
        out: &mut Output,
    ) {
        let Some(root) = message.thread_root else {
            return;
        };
        let Some(as_of) = self.counting_as_of(root) else {
            return;
        };
        if event_seq <= as_of || self.my_id() == Some(message.author_id) {
            return;
        }
        let Some(last_read) = self.thread_states.get(&root).map(|state| state.last_read) else {
            return;
        };
        if message.seq <= last_read {
            return;
        }
        let counted = self.reply_was_counted(root, message.seq, as_of);
        let tracked = self
            .thread_counted
            .get_mut(&root)
            .and_then(|counted| counted.remove(&message.seq));
        let Some(state) = self.thread_states.get_mut(&root) else {
            return;
        };
        if !counted || state.unread_count == 0 {
            return;
        }
        state.unread_count -= 1;
        let mentioned = tracked.unwrap_or_else(|| {
            self.me
                .as_ref()
                .is_some_and(|me| had_mentions.includes(me.id))
        });
        if mentioned {
            state.mention_count = state.mention_count.saturating_sub(1);
        }
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    /// Applies a reply's edit to the thread's mention count (see `count_message_edited`).
    pub(super) fn count_reply_edited(
        &mut self,
        message: &Message,
        previous: &Mentions,
        event_seq: EventSeq,
        out: &mut Output,
    ) {
        let Some(root) = message.thread_root else {
            return;
        };
        let Some(as_of) = self.counting_as_of(root) else {
            return;
        };
        let Some(me) = self.my_id() else {
            return;
        };
        if event_seq <= as_of || me == message.author_id {
            return;
        }
        let Some(last_read) = self.thread_states.get(&root).map(|state| state.last_read) else {
            return;
        };
        let seq = message.seq;
        if seq <= last_read || !self.reply_was_counted(root, seq, as_of) {
            return;
        }
        let tracked = self
            .thread_counted
            .get(&root)
            .and_then(|counted| counted.get(&seq))
            .copied();
        let Some(state) = self.thread_states.get_mut(&root) else {
            return;
        };
        let before = previous.includes(me);
        let held = tracked.unwrap_or(before);
        let now = if message.deleted {
            before
        } else {
            message.mentions.includes(me)
        };
        self.thread_counted
            .entry(root)
            .or_default()
            .insert(seq, now);
        if now == held {
            return;
        }
        state.mention_count = if now {
            state.mention_count.saturating_add(1)
        } else {
            state.mention_count.saturating_sub(1)
        };
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    // ----- Marking read -----

    /// Evaluated after every input: asks the server to mark the open thread read when the person
    /// is looking at its newest reply.
    pub(super) fn maybe_mark_thread_read(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || !self.window_active {
            return;
        }
        let Some(pane) = &self.pane else {
            return;
        };
        let root = pane.root;
        if !self.thread_at_bottom.contains(&root) || self.thread_mark_in_flight.contains_key(&root)
        {
            return;
        }
        let Some(state) = self.thread_states.get(&root) else {
            return;
        };
        if state.unread_count == 0 || !pane.replies.initial_loaded {
            return;
        }
        let Some(newest) = pane.replies.newest_seq() else {
            return;
        };
        if newest <= state.last_read
            || self
                .thread_mark_failed
                .get(&root)
                .is_some_and(|failed| *failed >= newest)
        {
            return;
        }
        self.thread_mark_in_flight.insert(root, newest);
        out.effects.push(Effect::MarkThreadRead {
            root,
            up_to: newest,
        });
    }

    pub(super) fn thread_read_result(
        &mut self,
        root: MessageId,
        up_to: EventSeq,
        result: Result<lazy_protocol::ThreadStateResponse, RequestError>,
        out: &mut Output,
    ) {
        self.thread_mark_in_flight.remove(&root);
        let reported = match result {
            Ok(response) => response.state,
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(RequestError::Rejected(error))
                if error.code == lazy_protocol::ErrorCode::NotFound =>
            {
                return self.thread_gone(root, out);
            }
            Err(_) => {
                self.thread_mark_failed.insert(root, up_to);
                return;
            }
        };
        let Some(me) = self.my_id() else {
            return;
        };
        let Some(current) = self.thread_states.get(&root) else {
            return;
        };
        let conversation = current.conversation_id;
        let last_read = current.last_read.max(reported.last_read);
        let current_unread = current.unread_count;
        // Replies that were read stop counting, and the entries that tracked them are done.
        let mut read_count = 0u32;
        let mut read_mentions = 0u32;
        if let Some(counted) = self.thread_counted.get_mut(&root) {
            let read: Vec<(EventSeq, bool)> = counted
                .range(..=last_read)
                .map(|(seq, mentioning)| (*seq, *mentioning))
                .collect();
            for (seq, mentioning) in read {
                counted.remove(&seq);
                read_count += 1;
                read_mentions += u32::from(mentioning);
            }
        }
        // As for conversations: the answer's own count describes the server's log when it
        // answered, which can be ahead of the events applied here, so it is used only through
        // the rows. With every reply after the read position loaded, the rows are the count.
        let applied = self
            .conversation(conversation)
            .and_then(|c| self.applied.get(&c.workspace_id))
            .copied()
            .unwrap_or(EventSeq::ZERO);
        let through = applied.max(
            self.thread_as_of
                .get(&root)
                .copied()
                .unwrap_or(EventSeq::ZERO),
        );
        let rows = match &self.pane {
            Some(pane) if pane.root == root && pane.replies.is_complete_after(last_read) => {
                Some(pane.replies.others_between(last_read, through, me))
            }
            _ => None,
        };
        let unread = match &rows {
            Some(rows) => {
                let counted = self.thread_counted.entry(root).or_default();
                counted.retain(|seq, _| *seq > through);
                counted.extend(rows.iter().copied());
                self.thread_rebuilt.insert(root, through);
                u32::try_from(rows.len()).unwrap_or(u32::MAX)
            }
            None => current_unread.saturating_sub(read_count),
        };
        if let Some(state) = self.thread_states.get_mut(&root) {
            state.last_read = last_read;
            state.unread_count = unread;
            state.mention_count = match &rows {
                Some(rows) => {
                    u32::try_from(rows.iter().filter(|(_, mentioning)| *mentioning).count())
                        .unwrap_or(u32::MAX)
                }
                None => state
                    .mention_count
                    .saturating_sub(read_mentions)
                    .min(unread),
            };
        }
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    // ----- Following -----

    pub(super) fn set_thread_follow(&mut self, root: MessageId, following: bool, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(conversation) = self.thread_conversation(root) else {
            return;
        };
        if self.conversation(conversation).is_none() {
            return;
        }
        let shown = self
            .thread_states
            .get(&root)
            .is_some_and(|state| state.following);
        match self.follows.get_mut(&root) {
            Some(follow) => follow.desired = following,
            None if shown == following => return,
            None => {
                self.follows.insert(
                    root,
                    Follow {
                        conversation,
                        desired: following,
                        server: shown,
                        in_flight: None,
                    },
                );
            }
        }
        self.show_following(root, conversation, following, out);
        self.pump_follow(root, out);
    }

    /// Makes the thread's state show `following`, adding a state for a thread the client holds
    /// none for (the server's own arrives with the event that follows the change).
    fn show_following(
        &mut self,
        root: MessageId,
        conversation: ConversationId,
        following: bool,
        out: &mut Output,
    ) {
        let state = self
            .thread_states
            .entry(root)
            .or_insert_with(|| ThreadUserState {
                root_id: root,
                conversation_id: conversation,
                following,
                last_read: EventSeq::ZERO,
                unread_count: 0,
                mention_count: 0,
            });
        self.thread_as_of.entry(root).or_insert(EventSeq::ZERO);
        if state.following != following {
            state.following = following;
        }
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    /// Sends the request for a follow change that has none in flight, or finishes it if the
    /// server already has what the person wants.
    fn pump_follow(&mut self, root: MessageId, out: &mut Output) {
        let Some(follow) = self.follows.get_mut(&root) else {
            return;
        };
        if follow.in_flight.is_some() {
            return;
        }
        if follow.server == follow.desired {
            self.follows.remove(&root);
            return;
        }
        let desired = follow.desired;
        follow.in_flight = Some(FollowInFlight {
            following: desired,
            repeats: 0,
        });
        out.effects.push(Effect::SetThreadFollow {
            root,
            following: desired,
        });
    }

    pub(super) fn thread_follow_result(
        &mut self,
        root: MessageId,
        result: Result<lazy_protocol::ThreadStateResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(follow) = self.follows.get_mut(&root) else {
            return;
        };
        let Some(in_flight) = follow.in_flight.as_mut() else {
            return;
        };
        match result {
            Ok(response) => {
                follow.in_flight = None;
                follow.server = response.state.following;
                if follow.desired == follow.server {
                    self.follows.remove(&root);
                    self.drop_idle_placeholder(root, out);
                } else {
                    self.pump_follow(root, out);
                }
            }
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(RequestError::Rejected(error)) => {
                self.revert_follow(root, out);
                if error.code == lazy_protocol::ErrorCode::NotFound {
                    self.forget_thread(root);
                    out.changes.push(Change::ThreadStateChanged { root });
                    self.thread_gone(root, out);
                } else {
                    let notice = "Couldn't change whether you follow that thread.";
                    self.set_notice(Some(notice.to_owned()), out);
                }
            }
            Err(RequestError::Unknown(_)) => {
                if in_flight.repeats >= MAX_RESENDS {
                    self.revert_follow(root, out);
                    let notice = "Couldn't reach the server, so the change wasn't saved.";
                    self.set_notice(Some(notice.to_owned()), out);
                    return;
                }
                // The same request again, for the person's latest wish: it sets a state, so it is
                // right whatever became of the earlier attempt.
                in_flight.repeats += 1;
                in_flight.following = follow.desired;
                out.effects.push(Effect::SetThreadFollow {
                    root,
                    following: follow.desired,
                });
            }
        }
    }

    /// A state that was only shown for a follow change (it has no position) goes once the change
    /// is settled on "not following": the server has nothing for the thread.
    fn drop_idle_placeholder(&mut self, root: MessageId, out: &mut Output) {
        let placeholder = self.thread_as_of.get(&root) == Some(&EventSeq::ZERO);
        if placeholder
            && self
                .thread_states
                .get(&root)
                .is_some_and(|state| !state.following)
        {
            self.thread_states.remove(&root);
            self.thread_as_of.remove(&root);
            out.changes.push(Change::ThreadStateChanged { root });
            self.cache_changed(false);
        }
    }

    /// Gives up a follow change and shows what the server has.
    fn revert_follow(&mut self, root: MessageId, out: &mut Output) {
        let Some(follow) = self.follows.remove(&root) else {
            return;
        };
        if let Some(state) = self.thread_states.get_mut(&root) {
            state.following = follow.server;
            let placeholder = self.thread_as_of.get(&root) == Some(&EventSeq::ZERO);
            if placeholder && !state.following {
                self.thread_states.remove(&root);
                self.thread_as_of.remove(&root);
            }
        }
        out.changes.push(Change::ThreadStateChanged { root });
        self.cache_changed(false);
    }

    /// The server's follow flag for a state, for the cache: the display shows an unsaved change
    /// that must not outlive the session.
    pub(super) fn saved_following(&self, root: MessageId, shown: bool) -> bool {
        self.follows
            .get(&root)
            .map_or(shown, |follow| follow.server)
    }

    /// The session ended: follow changes cannot be saved, so the display returns to the
    /// server's flags.
    pub(super) fn drop_all_follows(&mut self, out: &mut Output) {
        let roots: Vec<MessageId> = self.follows.keys().copied().collect();
        for root in roots {
            self.revert_follow(root, out);
        }
    }
}
