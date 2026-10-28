//! The regenerable content cache: what the client shows immediately on the next start.
//!
//! A [`ContentCache`] is a consistent picture of what the client knew at one moment: the
//! directory from the last bootstrap, the newest confirmed messages of the conversations whose
//! history had been loaded, and the event-log position the whole picture is current to.
//!
//! # The invariant
//!
//! **The cursor never runs ahead of the messages it covers.** For every cached conversation, the
//! cached messages include every message of that conversation with a position between the oldest
//! cached message and the cursor. (Messages *beyond* the cursor may be present: replay will
//! deliver them again and deduplicate them.) Because the cursor and the messages are one value
//! written in one atomic file replacement, a restart can resume from the cursor without losing
//! anything: it subscribes after the cursor, and the server replays the rest.
//!
//! Only conversations whose history was actually loaded are cached. A conversation that has only
//! seen live events has a partial timeline, which would not satisfy the invariant, so it is left
//! out and fetches its newest page when opened, exactly as on a first run.
//!
//! The cache is disposable: it may be absent, trimmed, or discarded at any time, and the client
//! then behaves as on a first run.

use std::collections::HashSet;

use lazy_domain::{ConversationId, EventSeq, WorkspaceRole};
use lazy_protocol::{Conversation, Message, User, Workspace};
use serde::{Deserialize, Serialize};

/// Most messages cached per conversation (the newest ones).
pub const MAX_CACHED_MESSAGES: usize = 200;
/// Most conversations with cached messages. The least recently active are dropped first.
pub const MAX_CACHED_CONVERSATIONS: usize = 50;
/// Largest cache file, in bytes. The runtime enforces it by evicting with [`ContentCache::shrink`].
pub const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;

/// A consistent snapshot of one signed-in person's client state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentCache {
    /// Whose cache this is. A cache is shown only to the person who produced it.
    pub user: User,
    pub workspaces: Vec<CachedWorkspace>,
}

/// One workspace's part of the cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedWorkspace {
    pub workspace: Workspace,
    pub role: WorkspaceRole,
    /// Conversations the person could read at the last bootstrap.
    pub conversations: Vec<Conversation>,
    pub members: Vec<User>,
    /// The event position everything in this workspace is current to; resume after it.
    pub cursor: EventSeq,
    pub timelines: Vec<CachedTimeline>,
}

/// The newest messages of one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedTimeline {
    pub conversation: ConversationId,
    /// Confirmed messages, oldest first, strictly increasing positions.
    pub messages: Vec<Message>,
    /// Whether messages older than the oldest cached one exist.
    pub has_more: bool,
}

impl ContentCache {
    /// Normalizes and bounds the cache: drops timelines of unknown conversations and messages
    /// that do not belong to their timeline, orders and deduplicates messages, keeps the newest
    /// [`MAX_CACHED_MESSAGES`] per conversation (marking the timeline as having older messages),
    /// and keeps at most [`MAX_CACHED_CONVERSATIONS`] timelines. Eviction is deterministic.
    pub fn bounded(mut self) -> Self {
        for workspace in &mut self.workspaces {
            let known: HashSet<ConversationId> =
                workspace.conversations.iter().map(|c| c.id).collect();
            let mut seen = HashSet::new();
            workspace
                .timelines
                .retain(|t| known.contains(&t.conversation) && seen.insert(t.conversation));
            for timeline in &mut workspace.timelines {
                let conversation = timeline.conversation;
                timeline
                    .messages
                    .retain(|m| m.conversation_id == conversation);
                timeline.messages.sort_by_key(|m| m.seq);
                timeline
                    .messages
                    .dedup_by(|a, b| a.id == b.id || a.seq == b.seq);
                if timeline.messages.len() > MAX_CACHED_MESSAGES {
                    let excess = timeline.messages.len() - MAX_CACHED_MESSAGES;
                    timeline.messages.drain(..excess);
                    timeline.has_more = true;
                }
            }
        }
        while self.timeline_count() > MAX_CACHED_CONVERSATIONS && self.shrink() {}
        self
    }

    /// Number of conversations with cached messages.
    pub fn timeline_count(&self) -> usize {
        self.workspaces.iter().map(|w| w.timelines.len()).sum()
    }

    /// Total cached messages.
    pub fn message_count(&self) -> usize {
        self.workspaces
            .iter()
            .flat_map(|w| &w.timelines)
            .map(|t| t.messages.len())
            .sum()
    }

    /// Evicts the least recently active conversation timeline (the one whose newest cached
    /// message is oldest; empty timelines first; ties by conversation id). Dropping a timeline
    /// is always safe: that conversation simply fetches its newest page when next opened.
    /// Returns `false` when there is nothing left to evict.
    pub fn shrink(&mut self) -> bool {
        let victim = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(w, workspace)| {
                workspace
                    .timelines
                    .iter()
                    .enumerate()
                    .map(move |(t, timeline)| {
                        (
                            (
                                timeline.messages.last().map(|m| m.seq),
                                timeline.conversation,
                            ),
                            w,
                            t,
                        )
                    })
            })
            .min_by_key(|(key, _, _)| *key);
        match victim {
            Some((_, w, t)) => {
                self.workspaces[w].timelines.remove(t);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use lazy_domain::{
        ChannelName, ChannelVisibility, DisplayName, MessageDocument, MessageId, Timestamp, UserId,
        Username, WorkspaceId,
    };

    use super::*;

    fn user() -> User {
        User {
            id: UserId::generate(),
            username: Username::parse("tester").unwrap(),
            display_name: DisplayName::parse("Tester").unwrap(),
        }
    }

    fn conversation(workspace: WorkspaceId) -> Conversation {
        Conversation {
            id: ConversationId::generate(),
            workspace_id: workspace,
            name: ChannelName::parse("general").unwrap(),
            topic: None,
            visibility: ChannelVisibility::Public,
            archived: false,
        }
    }

    fn message(conversation: ConversationId, author: UserId, seq: i64) -> Message {
        Message {
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: author,
            document: MessageDocument::from_plain_text("m").unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
        }
    }

    fn cache_with(timelines: Vec<(Conversation, Vec<i64>)>) -> ContentCache {
        let me = user();
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "W".into(),
        };
        let conversations = timelines.iter().map(|(c, _)| c.clone()).collect();
        let timelines = timelines
            .iter()
            .map(|(c, seqs)| CachedTimeline {
                conversation: c.id,
                messages: seqs.iter().map(|s| message(c.id, me.id, *s)).collect(),
                has_more: false,
            })
            .collect();
        ContentCache {
            user: me.clone(),
            workspaces: vec![CachedWorkspace {
                workspace,
                role: WorkspaceRole::Member,
                conversations,
                members: vec![me],
                cursor: EventSeq::new(1000).unwrap(),
                timelines,
            }],
        }
    }

    #[test]
    fn long_timelines_keep_only_the_newest_messages_and_say_older_ones_exist() {
        let workspace = WorkspaceId::generate();
        let channel = conversation(workspace);
        let seqs: Vec<i64> = (1..=(MAX_CACHED_MESSAGES as i64 + 30)).collect();
        let cache = cache_with(vec![(channel, seqs)]).bounded();
        let timeline = &cache.workspaces[0].timelines[0];
        assert_eq!(timeline.messages.len(), MAX_CACHED_MESSAGES);
        assert!(timeline.has_more);
        assert_eq!(
            timeline.messages.last().unwrap().seq.get(),
            MAX_CACHED_MESSAGES as i64 + 30
        );
        assert_eq!(timeline.messages[0].seq.get(), 31);
    }

    #[test]
    fn a_timeline_within_the_limit_is_left_alone() {
        let channel = conversation(WorkspaceId::generate());
        let cache = cache_with(vec![(channel, vec![3, 1, 2])]).bounded();
        let timeline = &cache.workspaces[0].timelines[0];
        let seqs: Vec<_> = timeline.messages.iter().map(|m| m.seq.get()).collect();
        assert_eq!(seqs, [1, 2, 3], "messages are put in order");
        assert!(!timeline.has_more);
    }

    #[test]
    fn at_most_fifty_conversations_are_kept_and_the_least_recent_go_first() {
        let workspace = WorkspaceId::generate();
        let all: Vec<(Conversation, Vec<i64>)> = (0..60)
            .map(|i| (conversation(workspace), vec![i as i64 + 1]))
            .collect();
        let expected_dropped: Vec<ConversationId> = all[..10].iter().map(|(c, _)| c.id).collect();
        let cache = cache_with(all).bounded();
        assert_eq!(cache.timeline_count(), MAX_CACHED_CONVERSATIONS);
        let kept: HashSet<_> = cache.workspaces[0]
            .timelines
            .iter()
            .map(|t| t.conversation)
            .collect();
        assert!(
            expected_dropped.iter().all(|id| !kept.contains(id)),
            "the ten quietest are evicted"
        );
        assert_eq!(
            cache.workspaces[0].conversations.len(),
            60,
            "the directory itself is kept"
        );
    }

    #[test]
    fn eviction_is_deterministic() {
        let workspace = WorkspaceId::generate();
        let all: Vec<_> = (0..55)
            .map(|_| (conversation(workspace), vec![7]))
            .collect();
        let first = cache_with(all.clone());
        let mut second = first.clone();
        second.workspaces[0].timelines.reverse();
        let (a, b) = (first.bounded(), second.bounded());
        let ids = |c: &ContentCache| -> HashSet<ConversationId> {
            c.workspaces[0]
                .timelines
                .iter()
                .map(|t| t.conversation)
                .collect()
        };
        assert_eq!(
            ids(&a),
            ids(&b),
            "ties break by conversation id, not by input order"
        );
    }

    #[test]
    fn shrink_removes_one_timeline_at_a_time_until_none_is_left() {
        let workspace = WorkspaceId::generate();
        let (quiet, busy) = (conversation(workspace), conversation(workspace));
        let mut cache = cache_with(vec![(quiet.clone(), vec![1]), (busy.clone(), vec![5])]);
        assert!(cache.shrink());
        assert_eq!(cache.workspaces[0].timelines[0].conversation, busy.id);
        assert!(cache.shrink());
        assert!(!cache.shrink());
        assert_eq!(cache.timeline_count(), 0);
    }

    #[test]
    fn timelines_of_unknown_conversations_and_foreign_messages_are_dropped() {
        let workspace = WorkspaceId::generate();
        let channel = conversation(workspace);
        let mut cache = cache_with(vec![(channel.clone(), vec![1, 2])]);
        let stranger = ConversationId::generate();
        cache.workspaces[0].timelines.push(CachedTimeline {
            conversation: stranger,
            messages: vec![],
            has_more: false,
        });
        let foreign = message(stranger, cache.user.id, 3);
        cache.workspaces[0].timelines[0].messages.push(foreign);
        let cache = cache.bounded();
        assert_eq!(cache.timeline_count(), 1);
        assert_eq!(cache.message_count(), 2);
    }

    #[test]
    fn round_trips_through_json() {
        let channel = conversation(WorkspaceId::generate());
        let cache = cache_with(vec![(channel, vec![1, 2, 3])]);
        let json = serde_json::to_vec(&cache).unwrap();
        assert_eq!(
            serde_json::from_slice::<ContentCache>(&json).unwrap(),
            cache
        );
    }
}
