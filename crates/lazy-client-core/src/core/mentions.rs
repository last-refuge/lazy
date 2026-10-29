//! Mentions: what can be named in a conversation, and how mention counts follow edits and
//! deletions.
//!
//! # What can be mentioned
//!
//! The composer and the editor offer, and the parser accepts, exactly the same set (D14):
//!
//! * **people** of the conversation first, then, in a *public* channel only, other members of the
//!   workspace (naming them is allowed but notifies no one, so they are marked
//!   `in_conversation: false`). Never someone who cannot see a private channel, a direct
//!   conversation, or a group conversation;
//! * **user groups**, in every conversation;
//! * **`@channel`**, where the person may use it (`lazy_domain::mention::can_mention_channel`);
//! * never the person themself, and never a username that cannot be written as a handle (a
//!   reserved word, or one that ends in punctuation).
//!
//! [`ClientCore::mention_directory`] is what sending and editing parse with, built from the same
//! sources as [`ClientCore::mention_candidates`], so a typed `@alice` becomes a mention exactly
//! when autocomplete would have offered it.
//!
//! # Counting
//!
//! `mention_count` is the number of unread messages (from someone else, after the read position,
//! not deleted) that notify the person. It follows the discipline of the unread count
//! ([`ClientCore::count_message_deleted`]): the server's baseline is authoritative, and events
//! after it adjust the count only for messages that were actually counted. Each counted message
//! has an entry in `counted_unread` whose value says whether the client currently counts it as
//! mentioning the person:
//!
//! * a message counted from its live creation event starts with `mentions.includes(me)`;
//! * an edit moves the entry to the new mentions;
//! * a message that was counted by the baseline has no entry until an event reveals what the
//!   baseline held: the first edit's `previous_mentions`, or the deletion's `had_mentions`;
//! * an event's own `previous_mentions` / `had_mentions` is the state *before* it, so an event
//!   that the client could only see as a tombstone (the earlier events of a deleted message are
//!   served as tombstones) still lets the entry catch up; the deletion then removes whatever the
//!   entry says the count holds.

use std::collections::HashSet;

use lazy_domain::mention::{
    CHANNEL_HANDLE, can_mention_channel, handle_is_reserved, handle_is_valid,
};
use lazy_domain::{ChannelVisibility, ConversationKind, MapDirectory, MentionTarget};
use lazy_protocol::Mentions;

use super::*;
use crate::model::{MentionCandidate, MentionKind};

/// How well typed text matches a handle and a display name; lower is better.
fn match_class(query: &str, handle: &str, display_name: &str) -> Option<u8> {
    if query.is_empty() || handle.starts_with(query) {
        return Some(0);
    }
    if handle.contains(query) {
        return Some(1);
    }
    let name = display_name.to_lowercase();
    if name.starts_with(query) || name.split_whitespace().any(|word| word.starts_with(query)) {
        return Some(2);
    }
    name.contains(query).then_some(3)
}

/// The people and groups that may be named in a conversation, with the facts the ranking and the
/// directory both use.
struct Sources<'a> {
    /// People in the conversation, with a handle that can be written.
    members: Vec<(&'a User, String)>,
    /// Other workspace members, offered only in a public channel.
    others: Vec<(&'a User, String)>,
    groups: Vec<&'a UserGroup>,
    channel_allowed: bool,
}

impl ClientCore {
    /// Who can be named in `conversation`, or `None` if it is not one the person belongs to.
    fn mention_sources(&self, conversation: ConversationId) -> Option<Sources<'_>> {
        let conv = self.conversation(conversation)?;
        let me = self.my_id();
        let usable = |user: &User| -> Option<String> {
            let handle = user.username.as_str().to_ascii_lowercase();
            // An app is not a person: it is never offered, and a typed `@bot-…` stays text. A
            // deactivated person is not reachable, so a mention of them stays text too.
            (!user.is_bot
                && !user.deactivated
                && handle_is_valid(&handle)
                && !handle_is_reserved(&handle))
            .then_some(handle)
        };
        let public_channel =
            conv.kind == ConversationKind::Channel && conv.visibility == ChannelVisibility::Public;

        let listed = self.conversation_members(conversation);
        let in_conversation: HashSet<UserId> = listed
            .map(|users| users.iter().copied().collect())
            .unwrap_or_default();
        let mut members = Vec::new();
        for id in listed.unwrap_or_default() {
            if Some(*id) == me {
                continue;
            }
            if let Some(user) = self.users.get(id)
                && let Some(handle) = usable(user)
            {
                members.push((user, handle));
            }
        }
        let mut others = Vec::new();
        // Without the list we cannot tell who is outside the channel, so no one is offered.
        if public_channel && listed.is_some() {
            for id in self.members.get(&conv.workspace_id).into_iter().flatten() {
                if Some(*id) == me || in_conversation.contains(id) {
                    continue;
                }
                if let Some(user) = self.users.get(id)
                    && let Some(handle) = usable(user)
                {
                    others.push((user, handle));
                }
            }
        }
        let groups = self
            .groups
            .iter()
            .filter(|group| group.workspace_id == conv.workspace_id)
            .collect();
        Some(Sources {
            members,
            others,
            groups,
            channel_allowed: can_mention_channel(self.role_in(conv.workspace_id), conv.is_default),
        })
    }

    /// What to suggest for the text typed after an `@` in `conversation`, best first.
    ///
    /// Order: people in the conversation, then (public channels only) other members of the
    /// workspace marked `in_conversation: false`, then user groups, then `@channel` where the
    /// person may use it. Within each, a handle that starts with the text, then one that
    /// contains it, then a display name with a word that starts with it, then one that contains
    /// it, each by display name. The match ignores case. The person themself is never offered.
    /// An empty query lists everything. Empty for a conversation the person is not in.
    pub fn mention_candidates(
        &self,
        conversation: ConversationId,
        query: &str,
    ) -> Vec<MentionCandidate> {
        let Some(sources) = self.mention_sources(conversation) else {
            return Vec::new();
        };
        let query = query.trim_start_matches('@').trim().to_lowercase();
        let mut found = Vec::new();

        let people = |list: &[(&User, String)], in_conversation: bool| {
            let mut ranked: Vec<(u8, String, MentionCandidate)> = list
                .iter()
                .filter_map(|(user, handle)| {
                    let class = match_class(&query, handle, user.display_name.as_str())?;
                    Some((
                        class,
                        user.display_name.as_str().to_lowercase(),
                        MentionCandidate {
                            target: MentionTarget::User { id: user.id },
                            handle: handle.clone(),
                            display_name: user.display_name.as_str().to_owned(),
                            kind: MentionKind::User,
                            in_conversation,
                        },
                    ))
                })
                .collect();
            ranked.sort_by(|a, b| (a.0, &a.1, &a.2.handle).cmp(&(b.0, &b.1, &b.2.handle)));
            ranked.into_iter().map(|(_, _, candidate)| candidate)
        };
        found.extend(people(&sources.members, true));
        found.extend(people(&sources.others, false));

        let mut groups: Vec<(u8, String, MentionCandidate)> = sources
            .groups
            .iter()
            .filter_map(|group| {
                let handle = group.handle.as_str().to_ascii_lowercase();
                let class = match_class(&query, &handle, group.name.as_str())?;
                Some((
                    class,
                    group.name.as_str().to_lowercase(),
                    MentionCandidate {
                        target: MentionTarget::Group { id: group.id },
                        handle,
                        display_name: group.name.as_str().to_owned(),
                        kind: MentionKind::Group,
                        in_conversation: true,
                    },
                ))
            })
            .collect();
        groups.sort_by(|a, b| (a.0, &a.1, &a.2.handle).cmp(&(b.0, &b.1, &b.2.handle)));
        found.extend(groups.into_iter().map(|(_, _, candidate)| candidate));

        if sources.channel_allowed
            && match_class(&query, CHANNEL_HANDLE, CHANNEL_DESCRIPTION).is_some()
        {
            found.push(MentionCandidate {
                target: MentionTarget::Channel,
                handle: CHANNEL_HANDLE.to_owned(),
                display_name: CHANNEL_DESCRIPTION.to_owned(),
                kind: MentionKind::Channel,
                in_conversation: true,
            });
        }
        found
    }

    /// The directory that sending and editing in `conversation` parse `@name` with: everything
    /// [`ClientCore::mention_candidates`] can offer there, by handle. A name it does not know
    /// stays plain text.
    pub fn mention_directory(&self, conversation: ConversationId) -> MapDirectory {
        let mut directory = MapDirectory::new();
        let Some(sources) = self.mention_sources(conversation) else {
            return directory;
        };
        for (user, handle) in sources.members.iter().chain(&sources.others) {
            directory.insert(handle, MentionTarget::User { id: user.id });
        }
        for group in sources.groups {
            directory.insert(group.handle.as_str(), MentionTarget::Group { id: group.id });
        }
        if sources.channel_allowed {
            directory.insert(CHANNEL_HANDLE, MentionTarget::Channel);
        }
        // `:name:` is turned into a custom emoji exactly when autocomplete would offer it: the
        // live emoji of the conversation's workspace. (Nothing waits for a list of people here.)
        if let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) {
            for emoji in self
                .live_custom_emoji()
                .filter(|emoji| emoji.workspace_id == workspace)
            {
                directory.insert_emoji(&emoji.name, emoji.id);
            }
        }
        directory
    }
}

/// What `@channel` is called in a list of suggestions.
const CHANNEL_DESCRIPTION: &str = "Everyone in this conversation";

// ----- Counting -----

impl ClientCore {
    /// Applies an edit event to the recipient's mention count.
    ///
    /// `previous` is who the message notified before this edit and `event_seq` is the edit's own
    /// log position. Only a message that was counted as unread (see
    /// [`ClientCore::count_message_deleted`]) can change the count, and only by what the client
    /// currently holds for it: the entry's tracked value, or for a message the baseline counted
    /// that no event has touched yet, `previous` (the baseline held the state before the first
    /// edit after it). A tombstone edit does not say what the message notified afterwards, so
    /// the entry catches up to `previous` and waits for the next event.
    pub(super) fn count_message_edited(
        &mut self,
        message: &Message,
        previous: &Mentions,
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
        let Some(me) = self.my_id() else {
            return;
        };
        if event_seq <= as_of || me == message.author_id {
            return;
        }
        let Some(last_read) = self
            .user_states
            .get(&conversation)
            .map(|state| state.last_read)
        else {
            return;
        };
        let seq = message.seq;
        if seq <= last_read || !self.was_counted(conversation, seq, as_of) {
            return;
        }
        let tracked = self
            .counted_unread
            .get(&conversation)
            .and_then(|counted| counted.get(&seq))
            .copied();
        let Some(state) = self.user_states.get_mut(&conversation) else {
            return;
        };
        let before = previous.includes(me);
        let held = tracked.unwrap_or(before);
        let now = if message.deleted {
            before
        } else {
            message.mentions.includes(me)
        };
        self.counted_unread
            .entry(conversation)
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
        out.changes
            .push(Change::ConversationStateChanged { conversation });
        self.cache_changed(false);
    }
}
