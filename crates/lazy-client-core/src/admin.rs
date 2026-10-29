//! Workspace administration (slice 3F, D22): the invitations the screen reads.

use lazy_protocol::{Invitation, InviteCode};

use crate::notify::LoadStatus;

/// An invitation that was just created, with the code that is shown only now.
///
/// The code is held in memory only: until `Input::DismissCreatedInvitation` or the invitations are
/// closed. It is never saved, cached, or written to a log (its `Debug` prints `<redacted>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedInvite {
    pub invitation: Invitation,
    pub code: InviteCode,
}

/// The workspace's invitations, newest first (what the server lists: created in the last 30 days,
/// or still active).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitationsView {
    pub open: bool,
    pub items: Vec<Invitation>,
    pub status: LoadStatus,
    pub created: Option<CreatedInvite>,
}

impl Default for InvitationsView {
    fn default() -> Self {
        Self {
            open: false,
            items: Vec::new(),
            status: LoadStatus::Idle,
            created: None,
        }
    }
}
