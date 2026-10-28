//! Conversation kinds, participant sets, and the rules for managing conversations.
//!
//! These are pure rules over facts the server loads from storage. The server evaluates them for
//! every command; clients may use them only to decide what to offer.

use serde::{Deserialize, Serialize};

use crate::{ChannelVisibility, UserId, WorkspaceRole};

/// Longest channel topic, in characters.
pub const MAX_TOPIC_CHARS: usize = 250;
/// Longest channel description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1_000;
/// Most people in one direct or group conversation, including the person who opens it.
pub const MAX_PARTICIPANTS: usize = 9;

/// What a conversation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    /// A named conversation, public or private.
    Channel,
    /// Exactly two people.
    Direct,
    /// Three to [`MAX_PARTICIPANTS`] people, with no name.
    Group,
}

/// Why a participant list was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParticipantError {
    #[error("a conversation needs at least one other person")]
    TooFew,
    #[error("a conversation can have at most {MAX_PARTICIPANTS} people")]
    TooMany,
}

/// The exact set of people in a direct or group conversation, including whoever opened it.
///
/// Sorted and free of duplicates, so two requests naming the same people in any order produce
/// equal sets. That equality is what makes opening a conversation idempotent: the same set always
/// finds the same conversation. Adding or removing someone makes a *different* set, and therefore
/// a different, new conversation that starts with no history.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParticipantSet(Vec<UserId>);

impl ParticipantSet {
    /// Builds the set from the opener and the people they named. Duplicates, including the
    /// opener naming themselves, are ignored.
    pub fn new(
        opener: UserId,
        others: impl IntoIterator<Item = UserId>,
    ) -> Result<Self, ParticipantError> {
        let mut users: Vec<UserId> = std::iter::once(opener).chain(others).collect();
        users.sort();
        users.dedup();
        match users.len() {
            0 | 1 => Err(ParticipantError::TooFew),
            n if n > MAX_PARTICIPANTS => Err(ParticipantError::TooMany),
            _ => Ok(Self(users)),
        }
    }

    /// Two people make a direct conversation; more make a group.
    pub fn kind(&self) -> ConversationKind {
        if self.0.len() == 2 {
            ConversationKind::Direct
        } else {
            ConversationKind::Group
        }
    }

    /// The people, in a stable order.
    pub fn users(&self) -> &[UserId] {
        &self.0
    }

    pub fn contains(&self, user: UserId) -> bool {
        self.0.binary_search(&user).is_ok()
    }

    /// A stable text key for storage: the sorted ids joined with `,`.
    pub fn key(&self) -> String {
        self.0
            .iter()
            .map(|user| user.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// What the rules need to know about one person's relationship to one conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationFacts {
    /// `None` when the person is not in the conversation's workspace.
    pub workspace_role: Option<WorkspaceRole>,
    pub is_member: bool,
    pub is_creator: bool,
    pub kind: ConversationKind,
    pub visibility: ChannelVisibility,
    pub archived: bool,
    /// The workspace's default channel, which everyone belongs to.
    pub is_default: bool,
}

impl ConversationFacts {
    fn in_workspace(&self) -> bool {
        self.workspace_role.is_some()
    }

    fn is_admin(&self) -> bool {
        matches!(
            self.workspace_role,
            Some(WorkspaceRole::Owner | WorkspaceRole::Admin)
        )
    }

    fn is_channel(&self) -> bool {
        self.kind == ConversationKind::Channel
    }

    /// Joining is for public, unarchived channels the person is not in yet.
    pub fn can_join(&self) -> bool {
        self.in_workspace()
            && self.is_channel()
            && self.visibility == ChannelVisibility::Public
            && !self.archived
            && !self.is_member
    }

    /// Leaving is for channel members. Nobody leaves the default channel, and direct and group
    /// conversations cannot be left (start a new group without the person instead).
    pub fn can_leave(&self) -> bool {
        self.in_workspace() && self.is_channel() && self.is_member && !self.is_default
    }

    /// Adding people to a channel: a member may add anyone from the workspace; an admin or owner
    /// may also add to a public channel they have not joined. Archived channels are closed.
    pub fn can_add_members(&self) -> bool {
        self.in_workspace()
            && self.is_channel()
            && !self.archived
            && (self.is_member || (self.is_admin() && self.visibility == ChannelVisibility::Public))
    }

    /// Removing someone else: an admin, owner, or the channel's creator, never from the default
    /// channel and never from a direct or group conversation.
    pub fn can_remove_members(&self) -> bool {
        self.in_workspace()
            && self.is_channel()
            && !self.is_default
            && (self.is_admin() || (self.is_creator && self.is_member))
    }

    /// Changing the topic or description: any member of an unarchived channel.
    pub fn can_edit_details(&self) -> bool {
        self.in_workspace() && self.is_channel() && self.is_member && !self.archived
    }

    /// Renaming, archiving, and unarchiving: an admin, owner, or the channel's creator. The
    /// default channel can be renamed but never archived (see [`Self::can_archive`]).
    pub fn can_rename(&self) -> bool {
        self.in_workspace()
            && self.is_channel()
            && !self.archived
            && (self.is_admin() || self.is_creator)
    }

    pub fn can_archive(&self) -> bool {
        self.in_workspace()
            && self.is_channel()
            && !self.is_default
            && !self.archived
            && (self.is_admin() || self.is_creator)
    }

    pub fn can_unarchive(&self) -> bool {
        self.in_workspace()
            && self.is_channel()
            && self.archived
            && (self.is_admin() || self.is_creator)
    }
}

/// Whether `text` fits the topic limit.
pub fn topic_is_valid(text: &str) -> bool {
    text.chars().count() <= MAX_TOPIC_CHARS && !text.chars().any(|c| c.is_control())
}

/// Whether `text` fits the description limit. Descriptions may contain line breaks.
pub fn description_is_valid(text: &str) -> bool {
    text.chars().count() <= MAX_DESCRIPTION_CHARS
        && !text.chars().any(|c| c.is_control() && c != '\n')
}

#[cfg(test)]
mod tests {
    use ChannelVisibility::{Private, Public};
    use WorkspaceRole::{Admin, Member, Owner};

    use super::*;

    fn facts(
        role: Option<WorkspaceRole>,
        member: bool,
        creator: bool,
        vis: ChannelVisibility,
    ) -> ConversationFacts {
        ConversationFacts {
            workspace_role: role,
            is_member: member,
            is_creator: creator,
            kind: ConversationKind::Channel,
            visibility: vis,
            archived: false,
            is_default: false,
        }
    }

    #[test]
    fn participant_sets_ignore_order_and_duplicates() {
        let (a, b, c) = (UserId::generate(), UserId::generate(), UserId::generate());
        let one = ParticipantSet::new(a, [b, c]).unwrap();
        let two = ParticipantSet::new(c, [b, a, b]).unwrap();
        assert_eq!(one, two);
        assert_eq!(one.key(), two.key());
        assert_eq!(one.kind(), ConversationKind::Group);
        assert!(one.contains(b));
    }

    #[test]
    fn two_people_make_a_direct_conversation() {
        let (a, b) = (UserId::generate(), UserId::generate());
        assert_eq!(
            ParticipantSet::new(a, [b]).unwrap().kind(),
            ConversationKind::Direct
        );
    }

    #[test]
    fn participant_limits() {
        let me = UserId::generate();
        assert_eq!(ParticipantSet::new(me, []), Err(ParticipantError::TooFew));
        assert_eq!(ParticipantSet::new(me, [me]), Err(ParticipantError::TooFew));
        let others: Vec<UserId> = (0..MAX_PARTICIPANTS).map(|_| UserId::generate()).collect();
        assert_eq!(
            ParticipantSet::new(me, others.clone()),
            Err(ParticipantError::TooMany)
        );
        assert!(ParticipantSet::new(me, others[..MAX_PARTICIPANTS - 1].to_vec()).is_ok());
    }

    #[test]
    fn adding_a_person_makes_a_different_set() {
        let (a, b, c) = (UserId::generate(), UserId::generate(), UserId::generate());
        assert_ne!(
            ParticipantSet::new(a, [b]).unwrap(),
            ParticipantSet::new(a, [b, c]).unwrap()
        );
    }

    #[test]
    fn only_public_unjoined_channels_can_be_joined() {
        assert!(facts(Some(Member), false, false, Public).can_join());
        assert!(
            !facts(Some(Member), true, false, Public).can_join(),
            "already a member"
        );
        assert!(
            !facts(Some(Owner), false, false, Private).can_join(),
            "private needs an invitation"
        );
        assert!(
            !facts(None, false, false, Public).can_join(),
            "outside the workspace"
        );
        let mut archived = facts(Some(Member), false, false, Public);
        archived.archived = true;
        assert!(!archived.can_join());
    }

    #[test]
    fn nobody_leaves_the_default_channel_or_a_conversation_of_people() {
        let mut default = facts(Some(Member), true, false, Public);
        default.is_default = true;
        assert!(!default.can_leave());
        assert!(facts(Some(Member), true, false, Private).can_leave());
        assert!(
            !facts(Some(Member), false, false, Public).can_leave(),
            "not a member"
        );
        let mut direct = facts(Some(Member), true, false, Private);
        direct.kind = ConversationKind::Direct;
        assert!(!direct.can_leave());
    }

    #[test]
    fn members_add_people_and_admins_add_to_public_channels_they_have_not_joined() {
        assert!(facts(Some(Member), true, false, Private).can_add_members());
        assert!(!facts(Some(Member), false, false, Public).can_add_members());
        assert!(facts(Some(Admin), false, false, Public).can_add_members());
        assert!(
            !facts(Some(Admin), false, false, Private).can_add_members(),
            "no bypass of private channels"
        );
        let mut direct = facts(Some(Member), true, false, Private);
        direct.kind = ConversationKind::Group;
        assert!(
            !direct.can_add_members(),
            "adding people to a group makes a new conversation instead"
        );
    }

    #[test]
    fn removing_and_archiving_belong_to_admins_and_the_creator() {
        let creator = facts(Some(Member), true, true, Public);
        let plain = facts(Some(Member), true, false, Public);
        let admin = facts(Some(Admin), false, false, Public);
        assert!(creator.can_remove_members() && creator.can_archive() && creator.can_rename());
        assert!(!plain.can_remove_members() && !plain.can_archive() && !plain.can_rename());
        assert!(admin.can_remove_members() && admin.can_archive());
        let mut default = admin;
        default.is_default = true;
        assert!(!default.can_remove_members() && !default.can_archive());
        assert!(default.can_rename(), "the default channel can be renamed");
    }

    #[test]
    fn archived_channels_can_only_be_unarchived() {
        let mut archived = facts(Some(Admin), true, true, Public);
        archived.archived = true;
        assert!(archived.can_unarchive());
        assert!(
            !archived.can_archive() && !archived.can_edit_details() && !archived.can_add_members()
        );
        assert!(!facts(Some(Member), true, false, Public).can_unarchive());
    }

    #[test]
    fn topic_and_description_limits() {
        assert!(topic_is_valid(&"é".repeat(MAX_TOPIC_CHARS)));
        assert!(!topic_is_valid(&"é".repeat(MAX_TOPIC_CHARS + 1)));
        assert!(!topic_is_valid("line\nbreak"));
        assert!(description_is_valid("two\nlines"));
        assert!(!description_is_valid(
            &"x".repeat(MAX_DESCRIPTION_CHARS + 1)
        ));
    }
}
