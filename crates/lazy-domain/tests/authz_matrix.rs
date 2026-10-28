//! The authorization matrix from `llm-docs/AUTHORIZATION.md`, written out row by row.
//!
//! Every combination of workspace role, channel membership, visibility, and archived state is
//! listed with the expected answers, so a change to the rules has to change this table on purpose.
//! The expectations are literal, not recomputed from the rule, so the test cannot agree with a
//! bug by construction.

use lazy_domain::{ChannelAccess, ChannelVisibility, WorkspaceRole};

use ChannelVisibility::{Private, Public};
use WorkspaceRole::{Admin, Member, Owner};

/// role (None = not in the workspace), channel member, visibility, archived, can read, can post
type Row = (
    Option<WorkspaceRole>,
    bool,
    ChannelVisibility,
    bool,
    bool,
    bool,
);

#[rustfmt::skip]
const MATRIX: [Row; 32] = [
    // Not in the workspace: nothing, whatever else is true.
    (None,         false, Public,  false, false, false),
    (None,         false, Public,  true,  false, false),
    (None,         false, Private, false, false, false),
    (None,         false, Private, true,  false, false),
    (None,         true,  Public,  false, false, false),
    (None,         true,  Public,  true,  false, false),
    (None,         true,  Private, false, false, false),
    (None,         true,  Private, true,  false, false),
    // Members: public channels are readable; posting needs joining; archived is read-only.
    (Some(Member), false, Public,  false, true,  false),
    (Some(Member), false, Public,  true,  true,  false),
    (Some(Member), false, Private, false, false, false),
    (Some(Member), false, Private, true,  false, false),
    (Some(Member), true,  Public,  false, true,  true),
    (Some(Member), true,  Public,  true,  true,  false),
    (Some(Member), true,  Private, false, true,  true),
    (Some(Member), true,  Private, true,  true,  false),
    // Admins: the same as members. A workspace role never bypasses a private channel.
    (Some(Admin),  false, Public,  false, true,  false),
    (Some(Admin),  false, Public,  true,  true,  false),
    (Some(Admin),  false, Private, false, false, false),
    (Some(Admin),  false, Private, true,  false, false),
    (Some(Admin),  true,  Public,  false, true,  true),
    (Some(Admin),  true,  Public,  true,  true,  false),
    (Some(Admin),  true,  Private, false, true,  true),
    (Some(Admin),  true,  Private, true,  true,  false),
    // Owners: likewise.
    (Some(Owner),  false, Public,  false, true,  false),
    (Some(Owner),  false, Public,  true,  true,  false),
    (Some(Owner),  false, Private, false, false, false),
    (Some(Owner),  false, Private, true,  false, false),
    (Some(Owner),  true,  Public,  false, true,  true),
    (Some(Owner),  true,  Public,  true,  true,  false),
    (Some(Owner),  true,  Private, false, true,  true),
    (Some(Owner),  true,  Private, true,  true,  false),
];

#[test]
fn every_combination_matches_the_documented_matrix() {
    for (role, member, visibility, archived, can_read, can_post) in MATRIX {
        let access = ChannelAccess {
            workspace_role: role,
            is_channel_member: member,
            visibility,
            archived,
        };
        assert_eq!(
            access.can_read(),
            can_read,
            "read: role={role:?} member={member} {visibility:?} archived={archived}"
        );
        assert_eq!(
            access.can_post(),
            can_post,
            "post: role={role:?} member={member} {visibility:?} archived={archived}"
        );
    }
}

#[test]
fn the_matrix_covers_every_combination_exactly_once() {
    let mut seen = std::collections::HashSet::new();
    for (role, member, visibility, archived, _, _) in MATRIX {
        assert!(
            seen.insert((role, member, visibility, archived)),
            "duplicate row"
        );
    }
    // 4 roles (including "none") x 2 membership x 2 visibility x 2 archived.
    assert_eq!(seen.len(), 4 * 2 * 2 * 2);
}

#[test]
fn posting_implies_reading() {
    for (_, _, _, _, can_read, can_post) in MATRIX {
        assert!(
            !can_post || can_read,
            "a person who can post must be able to read"
        );
    }
}
