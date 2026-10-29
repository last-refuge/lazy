//! Who a message mentions: targets, handles, and the directory that turns a typed `@name` into one.
//!
//! A mention is stored in the document as a target (a person, a group, or the whole channel) plus
//! the handle it was written with. The target is what counts: people are notified by identity, and
//! a renamed group still mentions the same people. The handle is what the author typed, kept so
//! the message reads sensibly if the person or group has since gone away, and so the editor can
//! show the source again.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{GroupId, UserId, WorkspaceRole};

/// Fewest characters in a handle.
pub const MIN_HANDLE_LEN: usize = 2;
/// Most characters in a handle (the longest username).
pub const MAX_HANDLE_LEN: usize = 32;
/// Most mentions in one message.
pub const MAX_MENTIONS: usize = 50;
/// Handles that name something other than a person or a group. People cannot register them and
/// groups cannot use them. `here` and `everyone` are reserved for later and mean nothing yet.
pub const RESERVED_HANDLES: [&str; 3] = ["channel", "here", "everyone"];
/// The handle of the whole-conversation mention.
pub const CHANNEL_HANDLE: &str = "channel";

/// What a mention refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MentionTarget {
    /// One person.
    User { id: UserId },
    /// Everyone in a user group, as it is when the message is sent.
    Group { id: GroupId },
    /// Every member of the conversation, as it is when the message is sent (`@channel`).
    Channel,
}

/// Whether `handle` can be a handle: [`MIN_HANDLE_LEN`] to [`MAX_HANDLE_LEN`] lowercase ASCII
/// letters, digits, `.`, `_`, or `-`, starting and ending with a letter or digit. Ending with a
/// letter or digit is what lets `@alice.` and `@alice_` end a sentence or an italic span without
/// swallowing the punctuation.
pub fn handle_is_valid(handle: &str) -> bool {
    let count = handle.chars().count();
    (MIN_HANDLE_LEN..=MAX_HANDLE_LEN).contains(&count)
        && handle
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && handle.starts_with(|c: char| c.is_ascii_alphanumeric())
        && handle.ends_with(|c: char| c.is_ascii_alphanumeric())
}

/// Whether `handle` is one of [`RESERVED_HANDLES`].
pub fn handle_is_reserved(handle: &str) -> bool {
    RESERVED_HANDLES
        .iter()
        .any(|reserved| handle.eq_ignore_ascii_case(reserved))
}

/// Whether the characters of a typed handle can continue one (what the parser scans before it
/// asks a directory).
pub fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Whether a person may use `@channel` in a conversation. Anyone who can post may, except in the
/// default channel, where it reaches the whole workspace and is for admins and owners.
pub fn can_mention_channel(role: Option<WorkspaceRole>, in_default_channel: bool) -> bool {
    match role {
        None => false,
        Some(WorkspaceRole::Member) => !in_default_channel,
        Some(WorkspaceRole::Admin | WorkspaceRole::Owner) => true,
    }
}

/// Turns the text typed after an `@` into a mention target.
///
/// The composer and editor give the parser a directory of the people, groups, and the `@channel`
/// the sender may mention in this conversation; anything it does not resolve stays plain text.
/// That is what makes autocomplete permission-aware: someone who cannot see a private channel is
/// simply not in the directory for it.
pub trait MentionDirectory {
    /// The target and canonical (lowercase) handle for `typed`, compared case-insensitively.
    fn resolve(&self, typed: &str) -> Option<(MentionTarget, String)>;

    /// The custom emoji and canonical (lowercase) name for `:typed:`, if the workspace has one by
    /// that name. The same directory serves both, because both are names the composer turns into
    /// something richer than text.
    fn resolve_emoji(&self, _typed: &str) -> Option<(crate::EmojiId, String)> {
        None
    }
}

/// A directory that resolves nothing: text is never turned into mentions.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMentions;

impl MentionDirectory for NoMentions {
    fn resolve(&self, _typed: &str) -> Option<(MentionTarget, String)> {
        None
    }
}

/// A directory backed by a map of handles.
#[derive(Debug, Clone, Default)]
pub struct MapDirectory {
    entries: HashMap<String, MentionTarget>,
    emoji: HashMap<String, crate::EmojiId>,
}

impl MapDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `handle` (lowercased). A handle that is not valid, or is already taken, is ignored,
    /// so the first one added wins.
    pub fn insert(&mut self, handle: &str, target: MentionTarget) -> &mut Self {
        let handle = handle.to_ascii_lowercase();
        if handle_is_valid(&handle) {
            self.entries.entry(handle).or_insert(target);
        }
        self
    }

    /// Adds a custom emoji by name (lowercased). A name that is not well formed, or is already
    /// taken, is ignored.
    pub fn insert_emoji(&mut self, name: &str, id: crate::EmojiId) -> &mut Self {
        let name = name.to_ascii_lowercase();
        if crate::emoji::custom_name_is_well_formed(&name) {
            self.emoji.entry(name).or_insert(id);
        }
        self
    }

    /// How many handles (people, groups, `channel`) the directory holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.emoji.is_empty()
    }

    /// The handle for a target, if it is in the directory.
    pub fn handle_of(&self, target: MentionTarget) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, t)| **t == target)
            .map(|(handle, _)| handle.as_str())
    }
}

impl MentionDirectory for MapDirectory {
    fn resolve(&self, typed: &str) -> Option<(MentionTarget, String)> {
        let handle = typed.to_ascii_lowercase();
        self.entries.get(&handle).map(|target| (*target, handle))
    }

    fn resolve_emoji(&self, typed: &str) -> Option<(crate::EmojiId, String)> {
        let name = typed.to_ascii_lowercase();
        self.emoji.get(&name).map(|id| (*id, name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_lowercase_slugs_that_start_and_end_with_a_letter_or_digit() {
        for good in ["al", "alice", "a.b", "a_b", "a-b", "team-2", "x9", "a1b2c3"] {
            assert!(handle_is_valid(good), "{good}");
        }
        for bad in [
            "",
            "a",
            "Alice",
            "alice.",
            "alice_",
            "-alice",
            ".a",
            "a b",
            "a@b",
            "é",
            "ab\n",
            &"a".repeat(33),
        ] {
            assert!(!handle_is_valid(bad), "{bad:?}");
        }
        assert!(handle_is_valid(&"a".repeat(32)));
    }

    #[test]
    fn reserved_handles_are_found_whatever_their_case() {
        for reserved in ["channel", "here", "everyone", "Channel", "HERE"] {
            assert!(handle_is_reserved(reserved), "{reserved}");
        }
        assert!(!handle_is_reserved("channels"));
        assert!(!handle_is_reserved("alice"));
    }

    #[test]
    fn channel_mentions_are_open_except_in_the_default_channel() {
        use WorkspaceRole::*;
        assert!(can_mention_channel(Some(Member), false));
        assert!(!can_mention_channel(Some(Member), true));
        assert!(can_mention_channel(Some(Admin), true));
        assert!(can_mention_channel(Some(Owner), true));
        assert!(!can_mention_channel(None, false));
    }

    #[test]
    fn targets_have_a_tagged_wire_shape() {
        let id = UserId::generate();
        assert_eq!(
            serde_json::to_value(MentionTarget::User { id }).unwrap(),
            serde_json::json!({"type": "user", "id": id.to_string()})
        );
        assert_eq!(
            serde_json::to_value(MentionTarget::Channel).unwrap(),
            serde_json::json!({"type": "channel"})
        );
        let group = MentionTarget::Group {
            id: GroupId::generate(),
        };
        let json = serde_json::to_string(&group).unwrap();
        assert_eq!(serde_json::from_str::<MentionTarget>(&json).unwrap(), group);
    }

    #[test]
    fn a_map_directory_resolves_case_insensitively_and_first_wins() {
        let (a, b) = (UserId::generate(), UserId::generate());
        let mut directory = MapDirectory::new();
        directory
            .insert("Alice", MentionTarget::User { id: a })
            .insert("alice", MentionTarget::User { id: b })
            .insert("not valid!", MentionTarget::Channel)
            .insert("channel", MentionTarget::Channel);
        assert_eq!(directory.len(), 2);
        assert_eq!(
            directory.resolve("ALICE"),
            Some((MentionTarget::User { id: a }, "alice".to_owned()))
        );
        assert_eq!(directory.resolve("bob"), None);
        assert_eq!(directory.handle_of(MentionTarget::Channel), Some("channel"));
        assert_eq!(NoMentions.resolve("alice"), None);
    }
}
