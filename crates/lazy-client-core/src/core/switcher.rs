//! The quick switcher (slice 3E, D21): one text box that finds a conversation, a person, or a
//! place to go. A pure function of what the core holds; choosing a result is an input.

use std::cmp::Reverse;

use lazy_domain::fuzzy::fuzzy_match;
use lazy_domain::{ConversationId, ConversationKind, UserId};
use lazy_protocol::Conversation;

use crate::change::Output;
use crate::discovery::{
    MAX_RECENT_CONVERSATIONS, QUICK_SWITCHER_LIMIT, QuickSwitcherItem, QuickSwitcherTarget,
};

use super::ClientCore;

/// How much less a match on a username counts than the same match on a title.
const USERNAME_PENALTY: i32 = 10;

/// A result and what orders it.
struct Ranked {
    item: QuickSwitcherItem,
    score: i32,
    /// Conversations, then people, then places, when scores are equal.
    kind: u8,
}

impl ClientCore {
    /// What the quick switcher offers for `query`, best first, at most
    /// [`QUICK_SWITCHER_LIMIT`].
    ///
    /// With no text: the conversations visited most recently, then those with unread messages
    /// (muted ones only when they were visited). With text: the conversations, people with no
    /// conversation yet, and places ("Threads", "Saved", "Search") whose words the text matches,
    /// best match first with a bonus for conversations visited recently or with unread messages
    /// (not when muted); archived conversations come after live ones; the last result offers to
    /// browse channels matching the text.
    pub fn quick_switcher_results(&self, query: &str) -> Vec<QuickSwitcherItem> {
        let query = query.trim();
        if query.is_empty() {
            return self.switcher_without_text();
        }
        let Some(me) = self.my_id() else {
            return Vec::new();
        };
        let mut ranked: Vec<Ranked> = Vec::new();
        for conversation in &self.conversations {
            if let Some(found) = self.match_conversation(conversation, query) {
                ranked.push(found);
            }
        }
        // People the person has no direct conversation with yet.
        let with_conversation: std::collections::HashSet<UserId> = self
            .conversations
            .iter()
            .filter(|c| c.kind == ConversationKind::Direct)
            .flat_map(|c| c.participants.iter().copied())
            .collect();
        for user in self.members() {
            if user.id == me || user.is_bot || with_conversation.contains(&user.id) {
                continue;
            }
            let name = user.display_name.as_str();
            let username = format!("@{}", user.username);
            let by_name = fuzzy_match(query, name);
            let by_username = fuzzy_match(query, user.username.as_str());
            let (score, matched, subtitle) = match (by_name, by_username) {
                (Some(n), Some(u)) if u.score > n.score => (u.score, Vec::new(), Some(username)),
                (Some(n), _) => (n.score, n.positions, Some(username)),
                (None, Some(u)) => (u.score, Vec::new(), Some(username)),
                (None, None) => continue,
            };
            ranked.push(Ranked {
                item: QuickSwitcherItem {
                    target: QuickSwitcherTarget::Person(user.id),
                    title: name.to_owned(),
                    subtitle,
                    matched,
                    unread: 0,
                    mentions: 0,
                    muted: false,
                    archived: false,
                },
                score,
                kind: 1,
            });
        }
        for (title, target, unread) in [
            (
                "Threads",
                QuickSwitcherTarget::Threads,
                self.inbox_unread_total(),
            ),
            ("Saved", QuickSwitcherTarget::Saved, 0),
            ("Search", QuickSwitcherTarget::Search, 0),
        ] {
            if let Some(found) = fuzzy_match(query, title) {
                ranked.push(Ranked {
                    item: QuickSwitcherItem {
                        target,
                        title: title.to_owned(),
                        subtitle: None,
                        matched: found.positions,
                        unread,
                        mentions: 0,
                        muted: false,
                        archived: false,
                    },
                    score: found.score,
                    kind: 2,
                });
            }
        }
        ranked.sort_by(|a, b| {
            (
                a.item.archived,
                Reverse(a.score),
                a.kind,
                a.item.title.to_lowercase(),
            )
                .cmp(&(
                    b.item.archived,
                    Reverse(b.score),
                    b.kind,
                    b.item.title.to_lowercase(),
                ))
                .then_with(|| format!("{:?}", a.item.target).cmp(&format!("{:?}", b.item.target)))
        });
        let mut items: Vec<QuickSwitcherItem> = ranked
            .into_iter()
            .take(QUICK_SWITCHER_LIMIT - 1)
            .map(|ranked| ranked.item)
            .collect();
        items.push(QuickSwitcherItem {
            target: QuickSwitcherTarget::BrowseChannels {
                query: query.to_owned(),
            },
            title: format!("Browse channels matching \"{query}\""),
            subtitle: None,
            matched: Vec::new(),
            unread: 0,
            mentions: 0,
            muted: false,
            archived: false,
        });
        items
    }

    fn recency_rank(&self, id: ConversationId) -> Option<usize> {
        self.discovery.recent.iter().position(|held| *held == id)
    }

    fn switcher_item(&self, conversation: &Conversation) -> QuickSwitcherItem {
        let state = self.user_states.get(&conversation.id);
        let title = self.title_of(conversation);
        QuickSwitcherItem {
            target: QuickSwitcherTarget::Conversation(conversation.id),
            title: if conversation.kind == ConversationKind::Channel {
                format!("#{title}")
            } else {
                title
            },
            subtitle: None,
            matched: Vec::new(),
            unread: state.map_or(0, |s| s.unread_count),
            mentions: state.map_or(0, |s| s.mention_count),
            muted: self.is_muted(conversation.id),
            archived: conversation.archived,
        }
    }

    fn switcher_without_text(&self) -> Vec<QuickSwitcherItem> {
        let mut seen = std::collections::HashSet::new();
        let mut recent: Vec<QuickSwitcherItem> = Vec::new();
        for id in &self.discovery.recent {
            if let Some(conversation) = self.conversation(*id)
                && seen.insert(*id)
            {
                recent.push(self.switcher_item(conversation));
            }
        }
        let mut unread: Vec<QuickSwitcherItem> = self
            .conversations
            .iter()
            .filter(|c| !seen.contains(&c.id) && !c.archived && !self.is_muted(c.id))
            .map(|c| self.switcher_item(c))
            .filter(|item| item.unread > 0)
            .collect();
        unread.sort_by(|a, b| {
            (
                Reverse(a.mentions),
                Reverse(a.unread),
                a.title.to_lowercase(),
            )
                .cmp(&(
                    Reverse(b.mentions),
                    Reverse(b.unread),
                    b.title.to_lowercase(),
                ))
        });
        let (mut live, archived): (Vec<_>, Vec<_>) = recent
            .into_iter()
            .chain(unread)
            .partition(|item| !item.archived);
        live.extend(archived);
        live.truncate(QUICK_SWITCHER_LIMIT);
        live
    }

    /// A conversation as a result of `query`, if its name or a participant's name matches.
    fn match_conversation(&self, conversation: &Conversation, query: &str) -> Option<Ranked> {
        let mut item = self.switcher_item(conversation);
        // The title carries `#` for a channel; the text is matched against the name alone.
        let name = self.title_of(conversation);
        let offset = item.title.chars().count() - name.chars().count();
        let by_title = fuzzy_match(query, &name);
        let mut best: Option<(i32, Vec<usize>, Option<String>)> = by_title.map(|found| {
            (
                found.score,
                found.positions.iter().map(|at| at + offset).collect(),
                None,
            )
        });
        if conversation.kind != ConversationKind::Channel {
            let me = self.my_id();
            for user in conversation.participants.iter().filter(|u| Some(**u) != me) {
                let Some(known) = self.users.get(user) else {
                    continue;
                };
                if let Some(found) = fuzzy_match(query, known.username.as_str()) {
                    // A match on a username is a little weaker than one on the title, so a
                    // conversation named for the text comes before a group that merely has
                    // someone of that username in it.
                    let score = found.score - USERNAME_PENALTY;
                    if best.as_ref().is_none_or(|(held, ..)| score > *held) {
                        best = Some((score, Vec::new(), Some(format!("@{}", known.username))));
                    }
                }
            }
        }
        let (fuzzy, matched, subtitle) = best?;
        item.matched = matched;
        item.subtitle = subtitle;
        let mut score = fuzzy;
        if let Some(rank) = self.recency_rank(conversation.id) {
            score += i32::try_from(MAX_RECENT_CONVERSATIONS - rank).unwrap_or(0) * 3;
        }
        if !item.muted {
            if item.unread > 0 {
                score += 10;
            }
            if item.mentions > 0 {
                score += 10;
            }
        }
        Some(Ranked {
            item,
            score,
            kind: 0,
        })
    }

    pub(super) fn choose_quick_switcher(&mut self, target: QuickSwitcherTarget, out: &mut Output) {
        match target {
            QuickSwitcherTarget::Conversation(id) => {
                if self.conversation(id).is_some() {
                    self.permalink_selection_chosen();
                    self.select_conversation(id, out);
                }
            }
            QuickSwitcherTarget::Person(user) => self.open_conversation(vec![user], out),
            QuickSwitcherTarget::Threads => self.open_inbox(out),
            QuickSwitcherTarget::Saved => self.open_saved(out),
            QuickSwitcherTarget::Search => self.open_search(out),
            QuickSwitcherTarget::BrowseChannels { query } => {
                self.discovery_set_browse_query(query, out)
            }
        }
    }
}
