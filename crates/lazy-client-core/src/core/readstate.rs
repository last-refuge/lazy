//! Read state and the events that change what the person belongs to.
//!
//! # Unread counting
//!
//! The server computes each conversation's baseline (`ConversationUserState`) at a known log
//! position. From then on the client keeps it current by processing events in log order:
//!
//! * a message by someone else in a conversation the person belongs to adds one, unless it is at
//!   or before the read position;
//! * a message by the person moves the read position to it (the server does the same in the send
//!   transaction) and leaves nothing unread, because everything earlier has been read;
//! * `UserStateChanged` replaces the state, since it was computed at its own log position.
//!
//! Each state remembers the position it is current to (`state_as_of`), and events at or before
//! it are not counted again. That makes replay after a restart, a cached start, or a bootstrap
//! that learns of a conversation count each message exactly once.
//!
//! # Marking read
//!
//! A conversation is marked read when it is selected, the window is active, its newest message
//! is on screen, and it has unread messages. At most one request per conversation is in flight;
//! messages that arrive meanwhile are covered by the next request when the answer returns.

use lazy_domain::{ConversationKind, WorkspaceRole};
use lazy_protocol::{BootstrapResponse, Conversation, ConversationUserState, Mentions, Message};

use super::conversations::default_user_state;
use super::*;

impl ClientCore {
    /// Where the "New messages" divider goes in a conversation: the first message after the
    /// position the person had read when they opened it. The position is captured on selection
    /// and kept until they leave the conversation, so the divider stays put while the
    /// conversation is marked read underneath it. `None` when nothing was unread then, or the
    /// first unread message is not loaded.
    pub fn unread_marker(&self, conversation: ConversationId) -> Option<ItemKey> {
        let anchor = *self.marker_anchor.get(&conversation)?;
        self.timelines.get(&conversation)?.first_after(anchor)
    }

    pub(super) fn is_known(&self, id: ConversationId) -> bool {
        self.conversation(id).is_some()
    }

    /// Whether a message at `seq` was counted as unread: it has an entry, or the state counted it
    /// (it is at or before the state's position) and the count has not been rebuilt from the
    /// timeline since in a way that left it out.
    pub(super) fn was_counted(
        &self,
        conversation: ConversationId,
        seq: EventSeq,
        as_of: EventSeq,
    ) -> bool {
        self.counted_unread
            .get(&conversation)
            .is_some_and(|counted| counted.contains_key(&seq))
            || (seq <= as_of
                && seq
                    > self
                        .rebuilt_through
                        .get(&conversation)
                        .copied()
                        .unwrap_or(EventSeq::ZERO))
    }

    /// Forgets live-counted messages that the state's position now covers.
    pub(super) fn forget_counted_through(&mut self, conversation: ConversationId, seq: EventSeq) {
        self.rebuilt_through.remove(&conversation);
        if let Some(counted) = self.counted_unread.get_mut(&conversation) {
            counted.retain(|position, _| *position > seq);
            if counted.is_empty() {
                self.counted_unread.remove(&conversation);
            }
        }
    }

    pub(super) fn my_id(&self) -> Option<UserId> {
        self.me.as_ref().map(|user| user.id)
    }

    // ----- Selection and the marker -----

    /// The one place the selection changes. Leaving a conversation forgets its divider and its
    /// on-screen report; arriving captures the divider position.
    pub(super) fn change_selection(&mut self, next: Option<ConversationId>, out: &mut Output) {
        // A conversation of a workspace in the background is never the selected one.
        if self.selected == next || self.scope != self.active {
            return;
        }
        // The pane belongs to the conversation it was opened in.
        self.close_pane(out);
        if let Some(previous) = self.selected.take() {
            self.marker_anchor.remove(&previous);
            self.at_bottom.remove(&previous);
            self.mark_failed.remove(&previous);
            out.changes.push(Change::ConversationStateChanged {
                conversation: previous,
            });
        }
        self.selected = next;
        if let Some(conversation) = next {
            self.at_bottom.remove(&conversation);
            self.capture_marker(conversation);
            out.changes
                .push(Change::ConversationStateChanged { conversation });
            self.remember_selected(conversation, out);
            self.note_visited(conversation, out);
        }
        out.changes.push(Change::SelectionChanged);
    }

    fn capture_marker(&mut self, conversation: ConversationId) {
        self.marker_anchor.remove(&conversation);
        if let Some(state) = self.user_states.get(&conversation)
            && state.unread_count > 0
        {
            self.marker_anchor.insert(conversation, state.last_read);
        }
    }

    // ----- Baselines from a bootstrap -----

    /// Takes the read state out of a bootstrap. A fresh start replaces everything; a
    /// revalidation keeps the cached state (which replay will bring up to date) and only fills
    /// in conversations the cache did not know, current to the bootstrap's `head`.
    pub(super) fn adopt_bootstrap_states(&mut self, response: &BootstrapResponse, fresh: bool) {
        if fresh {
            self.counted_unread.clear();
            self.rebuilt_through.clear();
            self.user_states.clear();
            self.state_as_of.clear();
            self.marker_anchor.clear();
            self.mark_in_flight.clear();
            self.mark_failed.clear();
        }
        for workspace in &response.workspaces {
            for state in &workspace.user_state {
                let id = state.conversation_id;
                if self.is_known(id) && !self.user_states.contains_key(&id) {
                    self.user_states.insert(id, state.clone());
                    self.state_as_of.insert(id, workspace.head);
                }
            }
            for conversation in &workspace.conversations {
                let id = conversation.id;
                self.user_states
                    .entry(id)
                    .or_insert_with(|| default_user_state(id));
                self.state_as_of.entry(id).or_insert(workspace.head);
            }
        }
        let known = self.all_conversation_ids();
        self.user_states.retain(|id, _| known.contains(id));
        self.state_as_of.retain(|id, _| known.contains(id));
        self.marker_anchor.retain(|id, _| known.contains(id));
        if fresh && let Some(selected) = self.selected {
            self.capture_marker(selected);
        }
    }

    // ----- Counting -----

    /// Applies a message event to the sender's and the recipient's read state.
    pub(super) fn count_message_event(&mut self, message: &Message, out: &mut Output) {
        let own = self.my_id() == Some(message.author_id);
        // A reply that is not in the channel belongs to its thread's counts. And a reply never
        // moves the person's read position in the channel, not even their own that was also
        // sent there (it may have been written from a thread pane): it is neither read nor
        // unread for them.
        if message.thread_root.is_some() && (!message.in_channel || own) {
            return;
        }
        let conversation = message.conversation_id;
        let seq = message.seq;
        let Some(as_of) = self.state_as_of.get(&conversation).copied() else {
            return;
        };
        if seq <= as_of {
            return;
        }
        if message.deleted && !own {
            return;
        }
        let read_immediately = self.selected == Some(conversation)
            && self.window_active
            && self.at_bottom.contains(&conversation);
        let selected = self.selected == Some(conversation);
        let mentions_me = self.my_id().is_some_and(|me| message.mentions.includes(me));
        let state = self
            .user_states
            .entry(conversation)
            .or_insert_with(|| default_user_state(conversation));
        let previously_read = state.last_read;
        if own {
            state.last_read = state.last_read.max(seq);
            state.unread_count = 0;
            state.mention_count = 0;
            self.counted_unread.remove(&conversation);
        } else if seq > state.last_read {
            state.unread_count = state.unread_count.saturating_add(1);
            if mentions_me {
                state.mention_count = state.mention_count.saturating_add(1);
            }
            self.counted_unread
                .entry(conversation)
                .or_default()
                .insert(seq, mentions_me);
            // Something arrived that the person is not looking at: remember where the new
            // messages began, so the divider appears when they come back to it.
            if selected && !read_immediately {
                self.marker_anchor
                    .entry(conversation)
                    .or_insert(previously_read);
            }
        }
        out.changes
            .push(Change::ConversationStateChanged { conversation });
        // Counting one more message is part of a stream of live events, so it may be batched.
        self.cache_changed(false);
    }

    /// Applies a deletion event to the recipient's read state. A message that was counted as
    /// unread stops counting; one that was never counted changes nothing, whether or not the
    /// client has it on screen.
    ///
    /// `event_seq` is the deletion's own log position. At or before the state's position the
    /// state already excludes the message, because the server never counts a deleted one.
    /// Otherwise only someone else's message after the read position can have been counted (the
    /// person's own never are; everything at or before the read position has been read), and
    /// whether it was depends on where its creation, at `message.seq`, falls:
    ///
    /// * at or before the state's position, the state includes it (the server computed that
    ///   state while the message existed, since its deletion is later);
    /// * after it, it was counted if and only if its creation arrived as a live event that was
    ///   counted ([`ClientCore::counted_unread`]). A replay delivers the creation of a deleted
    ///   message as a tombstone, which is never counted, so the deletion that follows must not
    ///   subtract it.
    ///
    /// This makes the count the same whether the client saw the creation live or only through a
    /// replay.
    pub(super) fn count_message_deleted(
        &mut self,
        message: &Message,
        had_mentions: &Mentions,
        event_seq: EventSeq,
        out: &mut Output,
    ) {
        if message.thread_root.is_some() && !message.in_channel {
            return;
        }
        let conversation = message.conversation_id;
        let Some(as_of) = self.state_as_of.get(&conversation).copied() else {
            return;
        };
        if event_seq <= as_of || self.my_id() == Some(message.author_id) {
            return;
        }
        let Some(last_read) = self
            .user_states
            .get(&conversation)
            .map(|state| state.last_read)
        else {
            return;
        };
        if message.seq <= last_read {
            return;
        }
        // What the client counts for this message: its entry if it has one, otherwise (a message
        // the baseline counted that no event has touched) whatever it notified before it was
        // deleted.
        let counted = self.was_counted(conversation, message.seq, as_of);
        let tracked = self
            .counted_unread
            .get_mut(&conversation)
            .and_then(|counted| counted.remove(&message.seq));
        let Some(state) = self.user_states.get_mut(&conversation) else {
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
        out.changes
            .push(Change::ConversationStateChanged { conversation });
        self.cache_changed(false);
    }

    // ----- Events about conversations and membership -----

    pub(super) fn event_conversation_created(
        &mut self,
        conversation: Conversation,
        seq: EventSeq,
        out: &mut Output,
    ) {
        if self.role_in_known_workspace(&conversation).is_none() {
            return;
        }
        let id = conversation.id;
        if self.is_known(id) {
            self.replace_conversation(conversation, out);
        } else {
            self.adopt_conversation(conversation, out);
            // Everything up to the creation is history, not news.
            self.state_as_of.insert(id, seq);
        }
    }

    pub(super) fn event_conversation_updated(
        &mut self,
        conversation: Conversation,
        out: &mut Output,
    ) {
        self.replace_conversation(conversation, out);
    }

    pub(super) fn event_member_added(
        &mut self,
        conversation: Conversation,
        user_id: UserId,
        seq: EventSeq,
        out: &mut Output,
    ) {
        let id = conversation.id;
        let me = self.my_id() == Some(user_id);
        self.member_list_changed(id, user_id, true, out);
        if self.is_known(id) {
            self.replace_conversation(conversation, out);
            if me {
                self.start_reading_from(id, seq, out);
            }
        } else if me && self.role_in_known_workspace(&conversation).is_some() {
            self.adopt_conversation(conversation, out);
            self.start_reading_from(id, seq, out);
            // The channel's earlier messages were not delivered to this person, so fetch them.
            self.request_history(id, None, out);
        }
        if me {
            self.set_browse_joined(id, true, out);
        }
    }

    /// Joining a conversation reads everything before the join: the read position starts at the
    /// join, and the messages before it are not counted.
    fn start_reading_from(&mut self, id: ConversationId, seq: EventSeq, out: &mut Output) {
        if let Some(state) = self.user_states.get_mut(&id) {
            state.last_read = state.last_read.max(seq);
            state.unread_count = 0;
            state.mention_count = 0;
        }
        let as_of = self.state_as_of.entry(id).or_insert(seq);
        *as_of = (*as_of).max(seq);
        self.forget_counted_through(id, seq);
        out.changes
            .push(Change::ConversationStateChanged { conversation: id });
    }

    pub(super) fn event_member_removed(
        &mut self,
        id: ConversationId,
        user_id: UserId,
        out: &mut Output,
    ) {
        if !self.is_known(id) {
            return;
        }
        self.member_list_changed(id, user_id, false, out);
        if self.my_id() == Some(user_id) {
            let announce = !self.leaving.contains(&id);
            self.remove_conversation(id, announce, out);
            self.leaving.remove(&id);
        } else if let Some(conversation) = self.conversations.iter_mut().find(|c| c.id == id) {
            conversation.member_count = conversation.member_count.saturating_sub(1);
            out.changes.push(Change::DirectoryChanged);
            out.changes
                .push(Change::ConversationStateChanged { conversation: id });
            self.cache_changed(true);
        }
    }

    pub(super) fn event_user_state_changed(
        &mut self,
        state: ConversationUserState,
        seq: EventSeq,
        out: &mut Output,
    ) {
        let id = state.conversation_id;
        if !self.is_known(id) {
            return;
        }
        self.user_states.insert(id, state);
        self.state_as_of.insert(id, seq);
        self.forget_counted_through(id, seq);
        out.changes
            .push(Change::ConversationStateChanged { conversation: id });
        self.cache_changed(false);
    }

    fn role_in_known_workspace(&self, conversation: &Conversation) -> Option<WorkspaceRole> {
        self.workspaces
            .iter()
            .find(|info| info.workspace.id == conversation.workspace_id)
            .map(|info| info.role)
    }

    /// The person no longer belongs to a conversation: forget it and everything local about it.
    /// If it was open, the default conversation opens instead. `announce` says why in a notice,
    /// for removals the person did not ask for.
    pub(super) fn remove_conversation(
        &mut self,
        id: ConversationId,
        announce: bool,
        out: &mut Output,
    ) {
        let Some(conversation) = self.conversation(id) else {
            return;
        };
        let title = self.conversation_title(id);
        let label = if conversation.kind == ConversationKind::Channel {
            format!("#{title}")
        } else {
            title
        };

        self.conversations.retain(|c| c.id != id);
        self.notify_conversation_gone(id, out);
        self.discovery_conversation_gone(id, out);
        self.inbox_conversation_gone(id, out);
        self.user_states.remove(&id);
        self.state_as_of.remove(&id);
        self.counted_unread.remove(&id);
        self.rebuilt_through.remove(&id);
        self.marker_anchor.remove(&id);
        self.at_bottom.remove(&id);
        self.mark_in_flight.remove(&id);
        self.mark_failed.remove(&id);
        if self.timelines.remove(&id).is_some() {
            out.changes.push(Change::TimelineReset { conversation: id });
        }
        if self.drafts.remove(&id).is_some() {
            out.changes.push(Change::DraftChanged { conversation: id });
            self.persist_drafts(out);
        }
        // Messages still being sent here can never be delivered; their journal entries are done.
        let stranded: Vec<OperationId> = self
            .ops
            .iter()
            .filter(|(_, op)| op.conversation == id)
            .map(|(operation, _)| *operation)
            .collect();
        for operation in stranded {
            self.files_of_message_settled(operation, out);
            self.settle_operation(operation, out);
        }
        self.drop_edits_in(id, out);
        self.drop_reactions_in(id);
        self.forget_threads_of(id, out);
        self.forget_webhooks_of(id, out);
        let had_caret = self.draft_selections.remove(&id).is_some();
        self.last_selected.retain(|_, remembered| *remembered != id);
        if had_caret {
            self.persist_view(out);
        }
        self.member_lists.remove(&id);
        self.set_browse_joined(id, false, out);

        if self.selected == Some(id) {
            let next = self.default_selection();
            self.change_selection(next, out);
            if let Some(next) = next {
                self.ensure_history(next, out);
            }
        }
        out.changes.push(Change::DirectoryChanged);
        self.cache_changed(true);
        if announce {
            self.set_notice(Some(format!("You were removed from {label}")), out);
        }
    }

    // ----- Marking read -----

    /// Evaluated after every input: asks the server to mark the open conversation read when the
    /// person is looking at its newest message.
    pub(super) fn maybe_mark_read(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || !self.window_active {
            return;
        }
        let Some(conversation) = self.selected else {
            return;
        };
        if !self.at_bottom.contains(&conversation)
            || self.mark_in_flight.contains_key(&conversation)
        {
            return;
        }
        let Some(state) = self.user_states.get(&conversation) else {
            return;
        };
        if state.unread_count == 0 {
            return;
        }
        let Some(timeline) = self.timelines.get(&conversation) else {
            return;
        };
        if !timeline.initial_loaded {
            return;
        }
        let Some(newest) = timeline.newest_seq() else {
            return;
        };
        // Positions only move forward: nothing is sent unless it advances the read position, and
        // a request that failed is not repeated for the same position until something changes.
        if newest <= state.last_read
            || self
                .mark_failed
                .get(&conversation)
                .is_some_and(|failed| *failed >= newest)
        {
            return;
        }
        self.mark_in_flight.insert(conversation, newest);
        out.effects.push(Effect::MarkRead {
            conversation,
            up_to: newest,
        });
    }

    pub(super) fn mark_read_result(
        &mut self,
        conversation: ConversationId,
        up_to: EventSeq,
        result: Result<ConversationUserState, RequestError>,
        out: &mut Output,
    ) {
        self.mark_in_flight.remove(&conversation);
        let reported = match result {
            Ok(state) => state,
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(_) => {
                self.mark_failed.insert(conversation, up_to);
                return;
            }
        };
        let Some(me) = self.my_id() else {
            return;
        };
        let Some(current) = self.user_states.get(&conversation) else {
            return;
        };
        let last_read = current.last_read.max(reported.last_read);
        let current_unread = current.unread_count;
        // Messages that were read stop counting, and the entries that tracked them are done.
        let mut read_count = 0u32;
        let mut read_mentions = 0u32;
        if let Some(counted) = self.counted_unread.get_mut(&conversation) {
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
        // Messages that arrived since the request are in the timeline; count those after the new
        // position. Only messages up to the applied position count: later ones may already be in
        // the timeline (a history page can run ahead of the events) but will be counted when
        // their events arrive.
        //
        // Without a complete timeline the answer's own count is not used: it describes the
        // server's log at the moment it answered, which can be ahead of the events applied here
        // (a message deleted since is already left out, and the deletion event would then be
        // subtracted a second time). What was read and is known to have been counted is taken
        // off instead; anything else the request read is a message the baseline counted, which
        // the `UserStateChanged` event the request produces accounts for.
        let applied = self
            .conversation(conversation)
            .and_then(|c| self.applied.get(&c.workspace_id))
            .copied()
            .unwrap_or(EventSeq::ZERO);
        // With every message after the read position loaded, the rows are the count, and they
        // also say which of the messages mention the person. The entries that track the messages
        // are made to agree with the rows: an event that arrives later for a row that is already
        // ahead of it then changes nothing, and a counted message that has no row (it was
        // deleted before the page was fetched) is no longer counted.
        let through = applied.max(
            self.state_as_of
                .get(&conversation)
                .copied()
                .unwrap_or(EventSeq::ZERO),
        );
        let rows = match self.timelines.get(&conversation) {
            Some(timeline) if timeline.is_complete_after(last_read) => {
                Some(timeline.others_between(last_read, through, me))
            }
            _ => None,
        };
        let unread = match &rows {
            Some(rows) => {
                let counted = self.counted_unread.entry(conversation).or_default();
                counted.retain(|seq, _| *seq > through);
                counted.extend(rows.iter().copied());
                self.rebuilt_through.insert(conversation, through);
                u32::try_from(rows.len()).unwrap_or(u32::MAX)
            }
            None => current_unread.saturating_sub(read_count),
        };
        if let Some(state) = self.user_states.get_mut(&conversation) {
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
            state.favorite = reported.favorite;
        }
        out.changes
            .push(Change::ConversationStateChanged { conversation });
        self.cache_changed(false);
    }
}
