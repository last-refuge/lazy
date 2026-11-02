//! One conversation's ordered rows, and the change notifications for editing them.
//!
//! Invariant: confirmed rows come first, sorted by event-log position; rows that are not
//! confirmed (pending, unknown, failed) follow in submission order. No two confirmed rows share a
//! message id.
//!
//! Reactions have a version of their own, `reactions_as_of`, independent of the content's
//! `revision`: a row keeps the reactions with the highest position however they arrive, so a late
//! history page never undoes a newer reaction. A row can also show the person's own reaction
//! changes that are still being saved; the server's list is kept aside until they are.
//!
//! A root's thread summary has a version of its own too (its `as_of`); a row keeps the summary with
//! the highest one however it arrives. A root that was deleted while it still has replies stays as
//! a tombstone row (`deleted`), and goes when its last reply does.
//!
//! A confirmed row always carries the newest revision the client has seen. A row whose edit is
//! being saved shows the new content while the server's content is kept aside, so a failed edit
//! can show the server's version again and the cache never records an unsaved one.

use std::collections::{HashMap, HashSet};

use lazy_domain::{
    ConversationId, EventSeq, MessageDocument, MessageId, OperationId, ReactionEmoji, UserId,
};
use lazy_protocol::{Attachment, Mentions, Message, ReactionSummary, ThreadSummary};

use crate::attachments::UploadProgress;
use crate::cache::CachedTimeline;
use crate::change::Change;
use crate::model::{Delivery, EditState, HistoryStatus, ItemKey, TimelineItem};

#[derive(Debug)]
pub(crate) struct Timeline {
    items: Vec<TimelineItem>,
    ids: HashSet<MessageId>,
    pub(crate) history: HistoryStatus,
    /// Identifies the most recent history request; older responses are discarded.
    pub(crate) generation: u64,
    /// Whether a history page has ever been merged since the last reset.
    pub(crate) initial_loaded: bool,
    /// Lower bound of the contiguous range loaded from history: everything from here up to the
    /// newest page is present. It is deliberately not the oldest row, because a message sent
    /// from this client can be confirmed at an older position than the pages loaded so far.
    pub(crate) history_floor: Option<EventSeq>,
    /// Whether messages older than the loaded range exist, as reported by the latest page.
    pub(crate) older_available: bool,
    /// The server's content of rows that currently show an unsaved edit.
    saved: HashMap<MessageId, MessageDocument>,
    /// Edits and deletions that arrived for messages not loaded yet, while a history page that
    /// may predate them was in flight. The page is merged with them applied; see
    /// [`Timeline::note_unloaded_update`].
    unloaded_updates: HashMap<MessageId, Message>,
    /// The same for reactions: the newest list that arrived for a message not loaded yet.
    unloaded_reactions: HashMap<MessageId, (EventSeq, Vec<ReactionSummary>)>,
    /// Rows that show the person's own unsaved reaction changes.
    reaction_overlays: HashMap<MessageId, ReactionOverlay>,
    /// For a root whose thread summary was cleared (its last reply was deleted): the position of
    /// that change, so a summary from an older page does not bring it back.
    none_since: HashMap<MessageId, EventSeq>,
    /// The newest thread change that arrived for a root not loaded yet, while a page that may
    /// predate it was in flight.
    unloaded_threads: HashMap<MessageId, (EventSeq, Option<ThreadSummary>)>,
}

/// What an update to a root's summary did to its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryOutcome {
    /// The row has this version or a newer one, or there is no row.
    Unchanged,
    Updated,
    /// The root was a tombstone and its last reply is gone, so the row was removed.
    RemovedRow,
}

/// What deleting a root did to its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeletedRoot {
    /// The row stays as a tombstone because the root still has replies.
    Tombstoned,
    /// The row is gone.
    Removed,
    /// There was no row, and none was added.
    Absent,
}

/// The person's reaction changes that are still being saved, laid over the server's reactions.
#[derive(Debug)]
struct ReactionOverlay {
    me: UserId,
    /// For each emoji, whether the person wants to have reacted.
    pending: Vec<(ReactionEmoji, bool)>,
    /// What the server has, which `item.reactions` would be without the pending changes.
    server: Vec<ReactionSummary>,
}

/// `server` with the person's pending changes applied. A new emoji goes last and the person
/// goes last among those who used an emoji, as the server orders them.
pub(crate) fn with_pending_reactions(
    server: &[ReactionSummary],
    me: UserId,
    pending: &[(ReactionEmoji, bool)],
) -> Vec<ReactionSummary> {
    let mut shown = server.to_vec();
    for (emoji, wanted) in pending {
        let position = shown.iter().position(|r| r.emoji == *emoji);
        match (position, wanted) {
            (Some(at), true) => {
                if !shown[at].users.contains(&me) {
                    shown[at].users.push(me);
                }
            }
            (None, true) => shown.push(ReactionSummary {
                emoji: *emoji,
                users: vec![me],
            }),
            (Some(at), false) => {
                shown[at].users.retain(|user| *user != me);
                if shown[at].users.is_empty() {
                    shown.remove(at);
                }
            }
            (None, false) => {}
        }
    }
    shown
}

impl Timeline {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            ids: HashSet::new(),
            history: HistoryStatus::NotLoaded,
            generation: 0,
            initial_loaded: false,
            history_floor: None,
            older_available: false,
            saved: HashMap::new(),
            unloaded_updates: HashMap::new(),
            unloaded_reactions: HashMap::new(),
            reaction_overlays: HashMap::new(),
            none_since: HashMap::new(),
            unloaded_threads: HashMap::new(),
        }
    }

    /// A timeline restored from the content cache: contiguous and current to the cache's cursor,
    /// so it counts as already loaded.
    ///
    /// The history floor is the bottom of the range known to be complete up to the cursor. It is
    /// not simply the oldest cached message: that message can lie beyond the cursor (a message
    /// this client sent and had acknowledged before the matching event arrived), and treating
    /// it as the floor would later exclude messages the replay delivers between the cursor and
    /// it.
    pub(crate) fn from_cache(cached: CachedTimeline, cursor: EventSeq) -> Self {
        let mut timeline = Self::new();
        for message in &cached.messages {
            if timeline.ids.insert(message.id) {
                timeline.items.push(Self::confirmed_row(message));
            }
        }
        timeline.history = HistoryStatus::Ready {
            has_more: cached.has_more,
        };
        timeline.initial_loaded = true;
        timeline.older_available = cached.has_more;
        timeline.history_floor = if cached.has_more {
            let oldest_covered = cached
                .messages
                .iter()
                .map(|m| m.seq)
                .find(|seq| *seq <= cursor);
            // With nothing cached at or below the cursor, the complete range starts just after it.
            oldest_covered.or_else(|| EventSeq::new(cursor.get() + 1))
        } else {
            // Nothing older exists, so every row belongs to the loaded range.
            None
        };
        timeline
    }

    /// The part of this timeline that can be cached, or `None` if its history was never loaded.
    ///
    /// Only the contiguous range from the history floor up is included: a message sent from this
    /// client can be confirmed below the loaded range, and caching it would claim contiguity the
    /// timeline does not have.
    pub(crate) fn to_cache(&self, conversation: ConversationId) -> Option<CachedTimeline> {
        if !self.initial_loaded {
            return None;
        }
        let messages = self
            .items
            .iter()
            .filter_map(|item| {
                let seq = item.seq()?;
                if self.history_floor.is_some_and(|floor| seq < floor) {
                    return None;
                }
                self.row_message(item, conversation)
            })
            .collect();
        Some(CachedTimeline {
            conversation,
            messages,
            has_more: self.older_available,
        })
    }

    pub(crate) fn items(&self) -> &[TimelineItem] {
        &self.items
    }

    /// The message a confirmed row stands for, as the server has it: an unsaved edit and the
    /// person's unsaved reaction changes are left out.
    fn row_message(&self, item: &TimelineItem, conversation: ConversationId) -> Option<Message> {
        let Delivery::Confirmed {
            message_id,
            seq,
            created_at,
            revision,
        } = item.delivery
        else {
            return None;
        };
        let document = self
            .saved
            .get(&message_id)
            .unwrap_or(&item.document)
            .clone();
        Some(Message {
            id: message_id,
            conversation_id: conversation,
            author_id: item.author_id,
            document,
            rich: item.rich.clone(),
            created_at,
            revision,
            seq,
            edited_at: item.edited_at,
            thread_root: item.thread_root,
            in_channel: item.in_channel,
            thread: item.thread.clone(),
            mentions: item.mentions.clone(),
            reactions: self
                .reaction_overlays
                .get(&message_id)
                .map_or_else(|| item.reactions.clone(), |overlay| overlay.server.clone()),
            reactions_as_of: item.reactions_as_of,
            deleted: item.deleted,
            attachments: item.attachments.clone(),
        })
    }

    /// The message `id` as the server has it, if a confirmed row holds it.
    pub(crate) fn message_of(
        &self,
        id: MessageId,
        conversation: ConversationId,
    ) -> Option<Message> {
        self.row_message(self.message_row(id)?, conversation)
    }

    /// Position of the newest confirmed row.
    pub(crate) fn newest_seq(&self) -> Option<EventSeq> {
        self.items.iter().rev().find_map(TimelineItem::seq)
    }

    /// Key of the first confirmed row after `position`: where a "New messages" divider goes.
    pub(crate) fn first_after(&self, position: EventSeq) -> Option<ItemKey> {
        self.items
            .iter()
            .find(|item| !item.deleted && item.seq().is_some_and(|seq| seq > position))
            .map(|item| item.key)
    }

    /// The confirmed rows with a position after `position` and no later than `up_to` that were
    /// written by someone other than `me`: each one's position and whether it mentions `me`.
    pub(crate) fn others_between(
        &self,
        position: EventSeq,
        up_to: EventSeq,
        me: UserId,
    ) -> Vec<(EventSeq, bool)> {
        self.items
            .iter()
            .filter(|item| item.author_id != me && !item.deleted)
            .filter_map(|item| {
                let seq = item.seq()?;
                (seq > position && seq <= up_to).then(|| (seq, item.mentions.includes(me)))
            })
            .collect()
    }

    /// Whether every message after `position` is loaded, so counting rows gives the true number.
    pub(crate) fn is_complete_after(&self, position: EventSeq) -> bool {
        self.initial_loaded
            && (!self.older_available
                || self
                    .history_floor
                    .is_none_or(|floor| position.get() + 1 >= floor.get()))
    }

    /// The `before` cursor for the next page of older history.
    pub(crate) fn older_cursor(&self) -> Option<EventSeq> {
        if self.initial_loaded {
            self.history_floor
        } else {
            None
        }
    }

    /// Records that a history page reaching down to `oldest` was merged. `has_more` says whether
    /// messages older than that page exist.
    pub(crate) fn note_page(&mut self, oldest: Option<EventSeq>, has_more: bool) {
        self.initial_loaded = true;
        self.older_available = has_more;
        if let Some(oldest) = oldest {
            self.history_floor = Some(self.history_floor.map_or(oldest, |floor| floor.min(oldest)));
        }
    }

    /// Index of the row with `key`. Searches from the end because recent rows are the common
    /// target.
    pub(crate) fn index_of(&self, key: ItemKey) -> Option<usize> {
        self.items.iter().rposition(|item| item.key == key)
    }

    /// Where a confirmed row with `seq` belongs.
    fn position_for(&self, seq: EventSeq) -> usize {
        self.items
            .partition_point(|item| item.seq().is_some_and(|existing| existing < seq))
    }

    fn confirmed_row(message: &Message) -> TimelineItem {
        TimelineItem {
            key: ItemKey::Message(message.id),
            author_id: message.author_id,
            document: message.document.clone(),
            rich: if message.deleted {
                None
            } else {
                message.rich.clone()
            },
            delivery: Delivery::Confirmed {
                message_id: message.id,
                seq: message.seq,
                created_at: message.created_at,
                revision: message.revision,
            },
            edited_at: message.edited_at,
            edit: EditState::None,
            reactions: message.reactions.clone(),
            reactions_as_of: message.reactions_as_of,
            mentions: message.mentions.clone(),
            thread: message.thread.clone(),
            thread_root: message.thread_root,
            in_channel: message.in_channel,
            deleted: message.deleted,
            // A deleted message's files can no longer be read.
            attachments: if message.deleted {
                Vec::new()
            } else {
                message.attachments.clone()
            },
            upload: None,
        }
    }

    /// Inserts messages that are not already present, keeping order. Adjacent insertions are
    /// reported as one splice, so a page of older history is a single splice at 0. Returns how
    /// many rows were added.
    pub(crate) fn merge_messages(
        &mut self,
        conversation: ConversationId,
        messages: &[Message],
        changes: &mut Vec<Change>,
    ) -> usize {
        let mut added = 0;
        let mut run: Option<(usize, usize)> = None;
        let flush = |run: Option<(usize, usize)>, changes: &mut Vec<Change>| {
            if let Some((at, count)) = run {
                changes.push(Change::TimelineSplice {
                    conversation,
                    at,
                    removed: 0,
                    inserted: count,
                });
            }
        };
        for message in messages {
            // A page can predate an edit, a deletion, or a reaction change whose event arrived
            // while it was in flight. Content and reactions are versioned separately.
            let mut merged = self
                .unloaded_updates
                .get(&message.id)
                .filter(|update| update.revision > message.revision)
                .cloned()
                .unwrap_or_else(|| message.clone());
            if message.reactions_as_of > merged.reactions_as_of {
                merged.reactions = message.reactions.clone();
                merged.reactions_as_of = message.reactions_as_of;
            }
            if let Some((as_of, list)) = self.unloaded_reactions.get(&message.id)
                && *as_of > merged.reactions_as_of
            {
                merged.reactions = list.clone();
                merged.reactions_as_of = *as_of;
            }
            // A root's summary. An update kept for the message (an edit or a deletion that arrived
            // while the page was in flight) was made after the page was read, so its summary,
            // even "no replies", is the later one unless the page's has a higher version. A
            // thread change that arrived meanwhile applies if it is newer than that.
            let from_update = merged.revision > message.revision;
            if let (true, Some(page), Some(update)) = (from_update, &message.thread, &merged.thread)
                && page.as_of > update.as_of
            {
                merged.thread = Some(page.clone());
            }
            let version = merged.thread.as_ref().map_or(EventSeq::ZERO, |s| s.as_of);
            if let Some((as_of, summary)) = self.unloaded_threads.get(&message.id)
                && *as_of > version
            {
                merged.thread = summary.clone();
                if summary.is_none() {
                    self.none_since.insert(message.id, *as_of);
                }
            }
            let message = &merged;
            if message.deleted {
                // A deleted root that still has replies stays as a placeholder; every other
                // deleted message is not shown.
                if message.thread_root.is_some() || message.thread.is_none() {
                    continue;
                }
                if !self.ids.insert(message.id) {
                    flush(run.take(), changes);
                    self.tombstone_root(conversation, message, changes);
                    continue;
                }
            } else if !self.ids.insert(message.id) {
                // Already loaded. The page may be newer than the row (a send confirmed from the
                // original response while the message was edited since).
                flush(run.take(), changes);
                self.apply_server_version(conversation, message, changes);
                continue;
            }
            let at = self.position_for(message.seq);
            self.items.insert(at, Self::confirmed_row(message));
            added += 1;
            run = match run {
                Some((start, count)) if at == start + count => Some((start, count + 1)),
                other => {
                    flush(other, changes);
                    Some((at, 1))
                }
            };
        }
        flush(run, changes);
        added
    }

    /// Appends a row for a message this client is sending.
    pub(crate) fn push_local(
        &mut self,
        conversation: ConversationId,
        item: TimelineItem,
        changes: &mut Vec<Change>,
    ) {
        debug_assert!(!item.is_confirmed());
        changes.push(Change::TimelineSplice {
            conversation,
            at: self.items.len(),
            removed: 0,
            inserted: 1,
        });
        self.items.push(item);
    }

    /// Turns the local row for `operation` into the confirmed `message`, moving it to its
    /// position. A standalone row for the same message (for example from a history page that
    /// arrived first) is dropped. Returns whether a local row for `operation` exists.
    pub(crate) fn confirm_local(
        &mut self,
        conversation: ConversationId,
        operation: OperationId,
        message: &Message,
        changes: &mut Vec<Change>,
    ) -> bool {
        let Some(mut index) = self.index_of(ItemKey::Operation(operation)) else {
            return false;
        };
        if self.items[index].is_confirmed() {
            return true;
        }
        // What the server has now: a standalone row for the same message may be newer than the
        // answer that confirms the send, which describes the message as it was committed.
        let mut latest = message.clone();
        if let Some(duplicate) = self.index_of(ItemKey::Message(message.id)) {
            let standalone = self.items.remove(duplicate);
            let saved = self.saved.remove(&message.id);
            let overlay = self.reaction_overlays.remove(&message.id);
            // The newer thread summary wins, however it arrived.
            if let Some(held) = &standalone.thread
                && latest
                    .thread
                    .as_ref()
                    .is_none_or(|known| known.as_of < held.as_of)
            {
                latest.thread = Some(held.clone());
            }
            if standalone.reactions_as_of > latest.reactions_as_of {
                latest.reactions_as_of = standalone.reactions_as_of;
                latest.reactions = overlay.map_or(standalone.reactions.clone(), |o| o.server);
            }
            if let Delivery::Confirmed { revision, .. } = standalone.delivery
                && revision > latest.revision
            {
                latest.revision = revision;
                latest.document = saved.unwrap_or(standalone.document);
                latest.rich = standalone.rich;
                latest.edited_at = standalone.edited_at;
                latest.mentions = standalone.mentions.clone();
            }
            changes.push(Change::TimelineSplice {
                conversation,
                at: duplicate,
                removed: 1,
                inserted: 0,
            });
            if duplicate < index {
                index -= 1;
            }
        }
        self.ids.insert(message.id);
        let mut item = self.items.remove(index);
        item.author_id = message.author_id;
        item.document = latest.document;
        item.rich = latest.rich;
        item.edited_at = latest.edited_at;
        item.mentions = latest.mentions;
        item.reactions = latest.reactions;
        item.reactions_as_of = latest.reactions_as_of;
        item.thread_root = message.thread_root;
        item.in_channel = message.in_channel;
        item.thread = latest.thread;
        item.attachments = if message.deleted {
            Vec::new()
        } else {
            message.attachments.clone()
        };
        item.upload = None;
        item.delivery = Delivery::Confirmed {
            message_id: message.id,
            seq: message.seq,
            created_at: message.created_at,
            revision: latest.revision,
        };
        let target = self.position_for(message.seq);
        self.items.insert(target, item);
        if target == index {
            changes.push(Change::TimelineItemUpdated {
                conversation,
                index,
            });
        } else {
            changes.push(Change::TimelineSplice {
                conversation,
                at: index,
                removed: 1,
                inserted: 0,
            });
            changes.push(Change::TimelineSplice {
                conversation,
                at: target,
                removed: 0,
                inserted: 1,
            });
        }
        true
    }

    /// Changes an unconfirmed row's delivery state in place.
    pub(crate) fn set_delivery(
        &mut self,
        conversation: ConversationId,
        key: ItemKey,
        delivery: Delivery,
        changes: &mut Vec<Change>,
    ) {
        debug_assert!(!matches!(delivery, Delivery::Confirmed { .. }));
        if let Some(index) = self.index_of(key) {
            if self.items[index].is_confirmed() {
                return;
            }
            self.items[index].delivery = delivery;
            changes.push(Change::TimelineItemUpdated {
                conversation,
                index,
            });
        }
    }

    /// Shows the files of an unconfirmed row and how far they have got.
    pub(crate) fn set_local_files(
        &mut self,
        conversation: ConversationId,
        key: ItemKey,
        attachments: Vec<Attachment>,
        upload: Option<UploadProgress>,
        changes: &mut Vec<Change>,
    ) {
        let Some(index) = self.index_of(key) else {
            return;
        };
        let item = &mut self.items[index];
        if item.is_confirmed() || (item.attachments == attachments && item.upload == upload) {
            return;
        }
        item.attachments = attachments;
        item.upload = upload;
        changes.push(Change::TimelineItemUpdated {
            conversation,
            index,
        });
    }

    /// Moves an unconfirmed row to the end, after every other row.
    pub(crate) fn move_to_back(
        &mut self,
        conversation: ConversationId,
        key: ItemKey,
        changes: &mut Vec<Change>,
    ) {
        let Some(index) = self.index_of(key) else {
            return;
        };
        if self.items[index].is_confirmed() || index + 1 == self.items.len() {
            return;
        }
        let item = self.items.remove(index);
        changes.push(Change::TimelineSplice {
            conversation,
            at: index,
            removed: 1,
            inserted: 0,
        });
        changes.push(Change::TimelineSplice {
            conversation,
            at: self.items.len(),
            removed: 0,
            inserted: 1,
        });
        self.items.push(item);
    }

    pub(crate) fn remove(
        &mut self,
        conversation: ConversationId,
        key: ItemKey,
        changes: &mut Vec<Change>,
    ) -> Option<TimelineItem> {
        let index = self.index_of(key)?;
        let item = self.items.remove(index);
        if let Some(id) = item.message_id() {
            self.ids.remove(&id);
            self.saved.remove(&id);
            self.reaction_overlays.remove(&id);
            self.none_since.remove(&id);
        }
        changes.push(Change::TimelineSplice {
            conversation,
            at: index,
            removed: 1,
            inserted: 0,
        });
        Some(item)
    }

    /// Index of the confirmed row for a message, whatever its key.
    pub(crate) fn index_of_message(&self, id: MessageId) -> Option<usize> {
        if !self.ids.contains(&id) {
            return None;
        }
        self.items
            .iter()
            .rposition(|item| item.message_id() == Some(id))
    }

    /// The row's key and revision, if the message is loaded.
    pub(crate) fn message_row(&self, id: MessageId) -> Option<&TimelineItem> {
        self.index_of_message(id).map(|index| &self.items[index])
    }

    /// The content the server has for a loaded message, not counting an unsaved edit.
    pub(crate) fn saved_document(&self, id: MessageId) -> Option<&MessageDocument> {
        self.saved
            .get(&id)
            .or_else(|| self.message_row(id).map(|item| &item.document))
    }

    /// Takes what is newer in a message from the server (an edit event, a response, a fetch, or
    /// a page that overlaps the row): its content if the revision is higher, and its reactions if
    /// `reactions_as_of` is higher. The two are versioned separately; older or equal versions
    /// change nothing, so duplicate and out-of-order deliveries are harmless. Returns whether
    /// the row changed.
    pub(crate) fn apply_server_version(
        &mut self,
        conversation: ConversationId,
        message: &Message,
        changes: &mut Vec<Change>,
    ) -> bool {
        let Some(index) = self.index_of_message(message.id) else {
            return false;
        };
        let item = &mut self.items[index];
        let Delivery::Confirmed { revision, .. } = &mut item.delivery else {
            return false;
        };
        let mut changed = false;
        if message.revision > *revision {
            *revision = message.revision;
            item.edited_at = message.edited_at;
            item.rich = if message.deleted {
                None
            } else {
                message.rich.clone()
            };
            item.mentions = message.mentions.clone();
            // Files never change, but a message the row first saw without them is complete now.
            if !message.deleted && item.attachments != message.attachments {
                item.attachments = message.attachments.clone();
            }
            match self.saved.get_mut(&message.id) {
                // The row shows an unsaved edit; the server's content moves underneath it.
                Some(saved) => *saved = message.document.clone(),
                None => item.document = message.document.clone(),
            }
            changed = true;
        }
        if let Some(summary) = &message.thread
            && item.thread.as_ref().map_or(
                self.none_since
                    .get(&message.id)
                    .copied()
                    .unwrap_or(EventSeq::ZERO),
                |s| s.as_of,
            ) < summary.as_of
        {
            item.thread = Some(summary.clone());
            self.none_since.remove(&message.id);
            changed = true;
        }
        if message.reactions_as_of > item.reactions_as_of {
            item.reactions_as_of = message.reactions_as_of;
            match self.reaction_overlays.get_mut(&message.id) {
                // The row shows the person's unsaved reaction changes over the server's list.
                Some(overlay) => {
                    overlay.server = message.reactions.clone();
                    item.reactions =
                        with_pending_reactions(&overlay.server, overlay.me, &overlay.pending);
                }
                None => item.reactions = message.reactions.clone(),
            }
            changed = true;
        }
        if changed {
            changes.push(Change::TimelineItemUpdated {
                conversation,
                index,
            });
        }
        changed
    }

    /// Takes a reaction list from the server, with the log position of the change that produced
    /// it (an event's own position), if that is newer than the row's. For a message that is not
    /// loaded, keeps it for a history page that may be about to bring the message in with an
    /// older list. Returns whether a row changed.
    pub(crate) fn apply_reactions(
        &mut self,
        conversation: ConversationId,
        id: MessageId,
        as_of: EventSeq,
        reactions: &[ReactionSummary],
        changes: &mut Vec<Change>,
    ) -> bool {
        let Some(index) = self.index_of_message(id) else {
            self.note_unloaded_reactions(id, as_of, reactions);
            return false;
        };
        let item = &mut self.items[index];
        if as_of <= item.reactions_as_of {
            return false;
        }
        item.reactions_as_of = as_of;
        match self.reaction_overlays.get_mut(&id) {
            Some(overlay) => {
                overlay.server = reactions.to_vec();
                item.reactions =
                    with_pending_reactions(&overlay.server, overlay.me, &overlay.pending);
            }
            None => item.reactions = reactions.to_vec(),
        }
        changes.push(Change::TimelineItemUpdated {
            conversation,
            index,
        });
        true
    }

    fn note_unloaded_reactions(
        &mut self,
        id: MessageId,
        as_of: EventSeq,
        list: &[ReactionSummary],
    ) {
        if self.history != HistoryStatus::Loading || self.ids.contains(&id) {
            return;
        }
        let newer = self
            .unloaded_reactions
            .get(&id)
            .is_none_or(|(known, _)| *known < as_of);
        if newer {
            self.unloaded_reactions.insert(id, (as_of, list.to_vec()));
        }
    }

    /// Shows the person's reaction changes that are still being saved on the row of a message:
    /// `pending` says, for each emoji, whether they want to have reacted. An empty list shows
    /// the server's reactions again.
    pub(crate) fn set_pending_reactions(
        &mut self,
        conversation: ConversationId,
        id: MessageId,
        me: UserId,
        pending: Vec<(ReactionEmoji, bool)>,
        changes: &mut Vec<Change>,
    ) {
        let Some(index) = self.index_of_message(id) else {
            return;
        };
        if pending.is_empty() && !self.reaction_overlays.contains_key(&id) {
            return;
        }
        let item = &mut self.items[index];
        let before = item.reactions.clone();
        if pending.is_empty() {
            if let Some(overlay) = self.reaction_overlays.remove(&id) {
                item.reactions = overlay.server;
            }
        } else {
            let server = self
                .reaction_overlays
                .remove(&id)
                .map_or_else(|| item.reactions.clone(), |overlay| overlay.server);
            item.reactions = with_pending_reactions(&server, me, &pending);
            self.reaction_overlays.insert(
                id,
                ReactionOverlay {
                    me,
                    pending,
                    server,
                },
            );
        }
        if item.reactions != before {
            changes.push(Change::TimelineItemUpdated {
                conversation,
                index,
            });
        }
    }

    /// Whether the server has `user` reacting with `emoji` on a loaded message, not counting
    /// the person's unsaved changes. `None` if the message is not loaded.
    pub(crate) fn server_reacted(
        &self,
        id: MessageId,
        user: UserId,
        emoji: ReactionEmoji,
    ) -> Option<bool> {
        let item = self.message_row(id)?;
        let list = self
            .reaction_overlays
            .get(&id)
            .map_or(item.reactions.as_slice(), |overlay| {
                overlay.server.as_slice()
            });
        Some(
            list.iter()
                .any(|reaction| reaction.emoji == emoji && reaction.users.contains(&user)),
        )
    }

    /// The version of a root's summary the row holds (zero before any).
    fn summary_version(&self, item: &TimelineItem) -> EventSeq {
        match (&item.thread, item.message_id()) {
            (Some(summary), _) => summary.as_of,
            (None, Some(id)) => self.none_since.get(&id).copied().unwrap_or(EventSeq::ZERO),
            (None, None) => EventSeq::ZERO,
        }
    }

    /// Takes a root's new thread summary (`None`: no replies remain), whose version is `as_of`,
    /// if that is newer than the row's. For a root that is not loaded, keeps it for a page that
    /// may be about to bring the root in with an older summary. A tombstone root whose last
    /// reply is gone is removed.
    pub(crate) fn apply_thread_summary(
        &mut self,
        conversation: ConversationId,
        root: MessageId,
        as_of: EventSeq,
        summary: Option<ThreadSummary>,
        changes: &mut Vec<Change>,
    ) -> SummaryOutcome {
        let Some(index) = self.index_of_message(root) else {
            self.note_unloaded_thread(root, as_of, summary);
            return SummaryOutcome::Unchanged;
        };
        if as_of <= self.summary_version(&self.items[index]) {
            return SummaryOutcome::Unchanged;
        }
        match summary {
            Some(summary) => {
                self.items[index].thread = Some(summary);
                self.none_since.remove(&root);
            }
            None if self.items[index].deleted => {
                let key = self.items[index].key;
                if let Some(removed) = self.remove(conversation, key, changes) {
                    // A page that was read before this must not bring the root back.
                    if let Some(tombstone) = self.row_message(&removed, conversation) {
                        self.note_unloaded_update(&tombstone);
                    }
                    self.note_unloaded_thread(root, as_of, None);
                }
                return SummaryOutcome::RemovedRow;
            }
            None => {
                self.items[index].thread = None;
                self.none_since.insert(root, as_of);
            }
        }
        changes.push(Change::TimelineItemUpdated {
            conversation,
            index,
        });
        SummaryOutcome::Updated
    }

    pub(crate) fn note_unloaded_thread(
        &mut self,
        root: MessageId,
        as_of: EventSeq,
        summary: Option<ThreadSummary>,
    ) {
        if self.history != HistoryStatus::Loading || self.ids.contains(&root) {
            return;
        }
        let newer = self
            .unloaded_threads
            .get(&root)
            .is_none_or(|(known, _)| *known < as_of);
        if newer {
            self.unloaded_threads.insert(root, (as_of, summary));
        }
    }

    /// A root was deleted. With replies left it stays as a placeholder (the better of the
    /// message's summary and the row's decides); without, its row goes. A root that is not
    /// loaded is added as a placeholder when it belongs to the loaded range.
    pub(crate) fn tombstone_root(
        &mut self,
        conversation: ConversationId,
        message: &Message,
        changes: &mut Vec<Change>,
    ) -> DeletedRoot {
        let Some(index) = self.index_of_message(message.id) else {
            // Before the first page has been merged nothing says the root is outside what it will
            // cover, and a page read before the deletion would not bring it in.
            let in_range =
                !self.initial_loaded || self.history_floor.is_none_or(|floor| message.seq >= floor);
            if message.thread.is_some() && message.thread_root.is_none() && in_range {
                self.ids.insert(message.id);
                let at = self.position_for(message.seq);
                self.items.insert(at, Self::confirmed_row(message));
                changes.push(Change::TimelineSplice {
                    conversation,
                    at,
                    removed: 0,
                    inserted: 1,
                });
                return DeletedRoot::Tombstoned;
            }
            return DeletedRoot::Absent;
        };
        let held = self.items[index].thread.clone();
        let best = match (held, message.thread.clone()) {
            (Some(a), Some(b)) => Some(if b.as_of > a.as_of { b } else { a }),
            (a, b) => a.or(b),
        };
        let Some(best) = best else {
            let key = self.items[index].key;
            self.remove(conversation, key, changes);
            return DeletedRoot::Removed;
        };
        let item = &mut self.items[index];
        item.deleted = true;
        item.document = message.document.clone();
        item.rich = None;
        item.attachments = Vec::new();
        item.edited_at = None;
        item.edit = EditState::None;
        item.mentions = Mentions::default();
        item.reactions = Vec::new();
        item.thread = Some(best);
        if let Delivery::Confirmed { revision, .. } = &mut item.delivery {
            *revision = (*revision).max(message.revision);
        }
        self.saved.remove(&message.id);
        self.reaction_overlays.remove(&message.id);
        self.none_since.remove(&message.id);
        changes.push(Change::TimelineItemUpdated {
            conversation,
            index,
        });
        DeletedRoot::Tombstoned
    }

    /// The reactions the server has on a loaded message, not counting the person's unsaved
    /// changes. `None` if the message is not loaded.
    pub(crate) fn server_reactions(&self, id: MessageId) -> Option<&[ReactionSummary]> {
        let item = self.message_row(id)?;
        Some(
            self.reaction_overlays
                .get(&id)
                .map_or(item.reactions.as_slice(), |overlay| {
                    overlay.server.as_slice()
                }),
        )
    }

    /// Takes a newer revision of a message from the server: onto its row if it is loaded, or else
    /// kept for the history page that may be about to bring the message in at an older revision.
    /// Returns whether a row changed.
    pub(crate) fn apply_update(
        &mut self,
        conversation: ConversationId,
        message: &Message,
        changes: &mut Vec<Change>,
    ) -> bool {
        if self.apply_server_version(conversation, message, changes) {
            return true;
        }
        self.note_unloaded_update(message);
        false
    }

    /// Sets what a row shows for an edit of its message. `saving` is the content to display
    /// while the edit is `Saving`; every other state shows the server's content.
    pub(crate) fn set_edit(
        &mut self,
        conversation: ConversationId,
        id: MessageId,
        edit: EditState,
        saving: Option<&MessageDocument>,
        changes: &mut Vec<Change>,
    ) {
        let Some(index) = self.index_of_message(id) else {
            return;
        };
        let item = &mut self.items[index];
        let before = (item.document.clone(), item.edit.clone());
        match (&edit, saving) {
            (EditState::Saving, Some(document)) => {
                self.saved
                    .entry(id)
                    .or_insert_with(|| item.document.clone());
                item.document = document.clone();
            }
            _ => {
                if let Some(saved) = self.saved.remove(&id) {
                    item.document = saved;
                }
            }
        }
        item.edit = edit;
        if before != (item.document.clone(), item.edit.clone()) {
            changes.push(Change::TimelineItemUpdated {
                conversation,
                index,
            });
        }
    }

    /// Remembers an edit or deletion for a message that is not loaded, if a history page that
    /// could predate it is in flight. The page is merged with the update applied. Returns whether
    /// it was kept.
    pub(crate) fn note_unloaded_update(&mut self, message: &Message) -> bool {
        if self.history != HistoryStatus::Loading || self.ids.contains(&message.id) {
            return false;
        }
        if !message.deleted {
            self.note_unloaded_reactions(message.id, message.reactions_as_of, &message.reactions);
        }
        let newer = self
            .unloaded_updates
            .get(&message.id)
            .is_none_or(|known| known.revision < message.revision);
        if newer {
            self.unloaded_updates.insert(message.id, message.clone());
        }
        newer
    }

    /// A history page was merged; the updates it may have predated have been applied.
    pub(crate) fn clear_unloaded_updates(&mut self) {
        self.unloaded_updates.clear();
        self.unloaded_reactions.clear();
        self.unloaded_threads.clear();
    }

    /// Discards all confirmed rows (they will be fetched again) but keeps rows that are still
    /// being delivered. Invalidates in-flight history requests.
    pub(crate) fn reset_keeping_local(
        &mut self,
        conversation: ConversationId,
        changes: &mut Vec<Change>,
    ) {
        self.items.retain(|item| !item.is_confirmed());
        self.ids.clear();
        self.saved.clear();
        self.unloaded_updates.clear();
        self.unloaded_reactions.clear();
        self.unloaded_threads.clear();
        self.none_since.clear();
        self.reaction_overlays.clear();
        self.history = HistoryStatus::NotLoaded;
        self.generation += 1;
        self.initial_loaded = false;
        self.history_floor = None;
        self.older_available = false;
        changes.push(Change::TimelineReset { conversation });
    }
}
