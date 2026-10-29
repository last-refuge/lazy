//! Rules for editing and deleting messages, and the permalink that points at one.
//!
//! These are pure rules over facts the server loads from storage. The server evaluates them for
//! every command; clients may use them only to decide what to offer.

use std::fmt;

use crate::{
    ChannelAccess, ConversationId, MessageDocument, MessageId, WorkspaceId, WorkspaceRole,
};

/// The text a deleted message carries in place of its content. Clients show their own wording
/// for a deleted message and never rely on this; it exists because every message has a valid
/// document.
pub const DELETED_PLACEHOLDER: &str = "This message was deleted.";

/// The document of a deleted message.
pub fn deleted_placeholder() -> MessageDocument {
    MessageDocument::from_plain_text(DELETED_PLACEHOLDER).expect("the placeholder is valid")
}

/// Most recent repliers a thread summary lists.
pub const MAX_THREAD_REPLIERS_LISTED: usize = 5;

/// What the rules need to know about one person's relationship to one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageFacts {
    /// The person's access to the conversation the message is in.
    pub access: ChannelAccess,
    pub is_author: bool,
    /// The message was already deleted. A deleted message is a tombstone: it keeps its place in
    /// the conversation but no longer has content.
    pub deleted: bool,
}

impl MessageFacts {
    fn is_admin(&self) -> bool {
        matches!(
            self.access.workspace_role,
            Some(WorkspaceRole::Owner | WorkspaceRole::Admin)
        )
    }

    /// Only the author edits, and only where they could post: an archived channel, or one they
    /// have left, is closed. A deleted message cannot be edited.
    pub fn can_edit(&self) -> bool {
        !self.deleted && self.is_author && self.access.can_post()
    }

    /// Replying to a thread: anyone who could post in the conversation, to a root that has not
    /// been deleted. (That the message is a root, not itself a reply, is for the server to check.)
    pub fn can_reply(&self) -> bool {
        !self.deleted && self.access.can_post()
    }

    /// Anyone who could post in the conversation may react to any message in it. Archived
    /// channels are closed, and a deleted message has nothing to react to.
    pub fn can_react(&self) -> bool {
        !self.deleted && self.access.can_post()
    }

    /// The author deletes their own messages wherever they could edit them. An admin or owner
    /// may delete anyone's message in a conversation they can read, including an archived one
    /// (moderation), but never reaches into a private channel they are not in.
    pub fn can_delete(&self) -> bool {
        !self.deleted
            && ((self.is_author && self.access.can_post())
                || (self.is_admin() && self.access.can_read()))
    }
}

/// The address of one message: `lazy://{workspace}/{conversation}/{message}`.
///
/// A permalink names a message without naming a server, so it keeps working wherever the person
/// signed in. Opening one selects the conversation and scrolls to the message, loading older
/// history if the message is not yet on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Permalink {
    pub workspace_id: WorkspaceId,
    pub conversation_id: ConversationId,
    pub message_id: MessageId,
}

impl Permalink {
    const SCHEME: &'static str = "lazy://";

    /// The permalink in `text`, or `None` if it is not exactly one. Extra path segments, a
    /// query, or a fragment make it something else.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(Self::SCHEME)?;
        let mut parts = rest.split('/');
        let workspace_id = parts.next()?.parse().ok()?;
        let conversation_id = parts.next()?.parse().ok()?;
        let message_id = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            workspace_id,
            conversation_id,
            message_id,
        })
    }
}

impl fmt::Display for Permalink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}/{}/{}",
            Self::SCHEME,
            self.workspace_id,
            self.conversation_id,
            self.message_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelVisibility::{Private, Public};
    use WorkspaceRole::{Admin, Member, Owner};

    fn facts(
        role: Option<WorkspaceRole>,
        member: bool,
        visibility: crate::ChannelVisibility,
        archived: bool,
        is_author: bool,
        deleted: bool,
    ) -> MessageFacts {
        MessageFacts {
            access: ChannelAccess {
                workspace_role: role,
                is_channel_member: member,
                visibility,
                archived,
            },
            is_author,
            deleted,
        }
    }

    #[test]
    fn only_the_author_edits_and_only_where_they_can_post() {
        assert!(facts(Some(Member), true, Public, false, true, false).can_edit());
        // Not the author: not even an owner.
        assert!(!facts(Some(Owner), true, Public, false, false, false).can_edit());
        // Archived, left, or not in the workspace.
        assert!(!facts(Some(Member), true, Public, true, true, false).can_edit());
        assert!(!facts(Some(Member), false, Public, false, true, false).can_edit());
        assert!(!facts(None, true, Public, false, true, false).can_edit());
        // Deleted.
        assert!(!facts(Some(Member), true, Public, false, true, true).can_edit());
    }

    #[test]
    fn authors_delete_their_own_messages_where_they_could_edit_them() {
        assert!(facts(Some(Member), true, Public, false, true, false).can_delete());
        assert!(!facts(Some(Member), true, Public, true, true, false).can_delete());
        assert!(!facts(Some(Member), false, Private, false, true, false).can_delete());
        assert!(!facts(Some(Member), true, Public, false, false, false).can_delete());
    }

    #[test]
    fn admins_moderate_what_they_can_read() {
        for role in [Admin, Owner] {
            // Public channels, joined or not, and archived ones.
            assert!(facts(Some(role), false, Public, false, false, false).can_delete());
            assert!(facts(Some(role), true, Public, true, false, false).can_delete());
            // Private channels they are in; never ones they are not.
            assert!(facts(Some(role), true, Private, false, false, false).can_delete());
            assert!(!facts(Some(role), false, Private, false, false, false).can_delete());
            // They still cannot edit someone else's words.
            assert!(!facts(Some(role), true, Public, false, false, false).can_edit());
        }
    }

    #[test]
    fn replying_needs_posting_and_a_root_that_still_exists() {
        assert!(facts(Some(Member), true, Public, false, false, false).can_reply());
        assert!(facts(Some(Member), true, Private, false, true, false).can_reply());
        assert!(!facts(Some(Member), false, Public, false, false, false).can_reply());
        assert!(!facts(Some(Owner), false, Public, false, false, false).can_reply());
        assert!(!facts(Some(Member), true, Public, true, false, false).can_reply());
        assert!(!facts(Some(Member), true, Public, false, false, true).can_reply());
        assert!(!facts(None, true, Public, false, false, false).can_reply());
    }

    #[test]
    fn anyone_who_can_post_can_react_to_anyone() {
        // Not the author, not an admin: a plain member reacts.
        assert!(facts(Some(Member), true, Public, false, false, false).can_react());
        assert!(facts(Some(Member), true, Private, false, true, false).can_react());
        // Reading is not enough, nor is a closed or deleted conversation.
        assert!(!facts(Some(Member), false, Public, false, false, false).can_react());
        assert!(!facts(Some(Owner), false, Public, false, false, false).can_react());
        assert!(!facts(Some(Member), true, Public, true, false, false).can_react());
        assert!(!facts(Some(Member), true, Public, false, false, true).can_react());
        assert!(!facts(None, true, Public, false, false, false).can_react());
    }

    #[test]
    fn a_deleted_message_cannot_be_deleted_or_edited_again() {
        let f = facts(Some(Owner), true, Public, false, true, true);
        assert!(!f.can_delete() && !f.can_edit());
    }

    #[test]
    fn the_placeholder_is_a_valid_document() {
        assert_eq!(deleted_placeholder().plain_text(), DELETED_PLACEHOLDER);
    }

    #[test]
    fn permalinks_round_trip() {
        let link = Permalink {
            workspace_id: WorkspaceId::generate(),
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
        };
        let text = link.to_string();
        assert!(text.starts_with("lazy://"));
        assert_eq!(Permalink::parse(&text), Some(link));
    }

    #[test]
    fn only_exact_permalinks_parse() {
        let link = Permalink {
            workspace_id: WorkspaceId::generate(),
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
        }
        .to_string();
        for bad in [
            String::new(),
            "lazy://".into(),
            "https://example.com".into(),
            format!("{link}/"),
            format!("{link}/extra"),
            format!("{link}?x=1"),
            format!("{link}#top"),
            format!(" {link}"),
            link.replacen("lazy", "other", 1),
            link.rsplit_once('/').unwrap().0.to_owned(),
        ] {
            assert_eq!(Permalink::parse(&bad), None, "{bad:?}");
        }
    }
}
