//! Pure authorization rules. The server evaluates these against facts it loads from Postgres
//! for every mutation, history read, and realtime delivery; clients may use them only to decide
//! what to offer, never to grant access.

use serde::{Deserialize, Serialize};

/// A person's role within one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRole {
    Member,
    Admin,
    Owner,
}

impl WorkspaceRole {
    /// Creating, changing, and deleting user groups: admins and owners.
    pub fn can_manage_groups(self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Adding, renaming, and deleting custom emoji: admins and owners.
    pub fn can_manage_emoji(self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Inviting people, changing the workspace's name and settings, and managing members (see
    /// [`crate::workspace`] for who may act on whom): admins and owners.
    pub fn can_manage_workspace(self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Creating and deleting integrations, such as incoming webhooks: admins and owners.
    pub fn can_manage_integrations(self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }
}

/// Most incoming webhooks one conversation can have.
pub const MAX_WEBHOOKS_PER_CONVERSATION: usize = 10;
/// Largest body an incoming webhook accepts, in bytes.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 65_536;

/// Most user groups in one workspace.
pub const MAX_GROUPS_PER_WORKSPACE: usize = 100;
/// Most people in one user group.
pub const MAX_GROUP_MEMBERS: usize = 200;

/// Who can find and read a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelVisibility {
    Public,
    Private,
}

/// One person's relationship to one channel, as loaded from storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelAccess {
    /// `None` when the person is not a member of the channel's workspace.
    pub workspace_role: Option<WorkspaceRole>,
    pub is_channel_member: bool,
    pub visibility: ChannelVisibility,
    pub archived: bool,
}

impl ChannelAccess {
    /// Whether the person may read history and receive live events.
    ///
    /// Workspace members read public channels; private channels need channel membership.
    /// Archived channels stay readable.
    pub fn can_read(&self) -> bool {
        self.workspace_role.is_some()
            && (self.visibility == ChannelVisibility::Public || self.is_channel_member)
    }

    /// Whether the person may post. Posting needs channel membership and an unarchived channel.
    pub fn can_post(&self) -> bool {
        self.workspace_role.is_some() && self.is_channel_member && !self.archived
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(
        role: Option<WorkspaceRole>,
        member: bool,
        vis: ChannelVisibility,
        archived: bool,
    ) -> ChannelAccess {
        ChannelAccess {
            workspace_role: role,
            is_channel_member: member,
            visibility: vis,
            archived,
        }
    }

    #[test]
    fn only_admins_and_owners_manage_groups_and_integrations() {
        assert!(!WorkspaceRole::Member.can_manage_groups());
        assert!(WorkspaceRole::Admin.can_manage_groups());
        assert!(WorkspaceRole::Owner.can_manage_groups());
        assert!(!WorkspaceRole::Member.can_manage_integrations());
        assert!(WorkspaceRole::Admin.can_manage_integrations());
        assert!(WorkspaceRole::Owner.can_manage_integrations());
    }

    #[test]
    fn non_members_of_the_workspace_get_nothing() {
        for vis in [ChannelVisibility::Public, ChannelVisibility::Private] {
            let a = access(None, true, vis, false);
            assert!(!a.can_read() && !a.can_post());
        }
    }

    #[test]
    fn public_channels_are_readable_but_not_postable_without_joining() {
        let a = access(
            Some(WorkspaceRole::Member),
            false,
            ChannelVisibility::Public,
            false,
        );
        assert!(a.can_read());
        assert!(!a.can_post());
    }

    #[test]
    fn private_channels_require_membership_even_for_admins() {
        let a = access(
            Some(WorkspaceRole::Owner),
            false,
            ChannelVisibility::Private,
            false,
        );
        assert!(!a.can_read() && !a.can_post());
        let a = access(
            Some(WorkspaceRole::Member),
            true,
            ChannelVisibility::Private,
            false,
        );
        assert!(a.can_read() && a.can_post());
    }

    #[test]
    fn archived_channels_are_read_only() {
        let a = access(
            Some(WorkspaceRole::Member),
            true,
            ChannelVisibility::Public,
            true,
        );
        assert!(a.can_read());
        assert!(!a.can_post());
    }
}
