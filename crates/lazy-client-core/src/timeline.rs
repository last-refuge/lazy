//! One conversation's ordered rows, and the change notifications for editing them.
//!
//! Invariant: confirmed rows come first, sorted by event-log position; rows that are not
//! confirmed (pending, unknown, failed) follow in submission order. No two confirmed rows share a
//! message id.

use std::collections::HashSet;

use lazy_domain::{ConversationId, EventSeq, MessageId, OperationId};
use lazy_protocol::Message;

use crate::cache::CachedTimeline;
use crate::change::Change;
use crate::model::{Delivery, HistoryStatus, ItemKey, TimelineItem};

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
                let Delivery::Confirmed {
                    message_id,
                    seq,
                    created_at,
                    revision,
                } = item.delivery
                else {
                    return None;
                };
                if self.history_floor.is_some_and(|floor| seq < floor) {
                    return None;
                }
                Some(Message {
                    id: message_id,
                    conversation_id: conversation,
                    author_id: item.author_id,
                    document: item.document.clone(),
                    created_at,
                    revision,
                    seq,
                })
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
            delivery: Delivery::Confirmed {
                message_id: message.id,
                seq: message.seq,
                created_at: message.created_at,
                revision: message.revision,
            },
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
            if !self.ids.insert(message.id) {
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
        if let Some(duplicate) = self.index_of(ItemKey::Message(message.id)) {
            self.items.remove(duplicate);
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
        item.document = message.document.clone();
        item.delivery = Delivery::Confirmed {
            message_id: message.id,
            seq: message.seq,
            created_at: message.created_at,
            revision: message.revision,
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
        }
        changes.push(Change::TimelineSplice {
            conversation,
            at: index,
            removed: 1,
            inserted: 0,
        });
        Some(item)
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
        self.history = HistoryStatus::NotLoaded;
        self.generation += 1;
        self.initial_loaded = false;
        self.history_floor = None;
        self.older_available = false;
        changes.push(Change::TimelineReset { conversation });
    }
}
