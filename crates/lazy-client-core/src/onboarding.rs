//! Joining a workspace for the first time (D29): what is known about the invitation code a person
//! typed, before they have chosen anything else.
//!
//! The screen asks the server what a code is for ([`lazy_protocol::InvitationPreview`]) without
//! using it, so that it can say "you're invited to Zebrafish Collective" before a password exists,
//! and so that a wrong code is found out at once. The code itself is never kept here: only what
//! the server said about it.

use std::fmt;

use lazy_protocol::InvitationPreview;

/// What is known about the code in the field.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum InvitationCheck {
    /// No code, or one not asked about yet.
    #[default]
    Idle,
    /// The server is being asked.
    Checking,
    /// The code is valid now. The names are the server's, shown to the person only.
    Valid(InvitationPreview),
    /// The code is wrong, expired, revoked, used up, or cannot be one. The server gives one answer
    /// for all of these and so does this.
    Invalid,
    /// The server takes no new accounts.
    Closed,
    /// The server is older than this check and cannot say; the code is checked when the account is
    /// created.
    Unchecked,
    /// The server could not be asked: in fixed words, never the code.
    Failed(String),
}

impl fmt::Debug for InvitationCheck {
    /// A workspace's name and a person's name are not printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("Idle"),
            Self::Checking => f.write_str("Checking"),
            Self::Valid(preview) => f
                .debug_struct("Valid")
                .field("kind", &preview.kind)
                .field("member_count", &preview.member_count)
                .finish_non_exhaustive(),
            Self::Invalid => f.write_str("Invalid"),
            Self::Closed => f.write_str("Closed"),
            Self::Unchecked => f.write_str("Unchecked"),
            Self::Failed(_) => f.write_str("Failed(..)"),
        }
    }
}

/// Why the server did not say what a code is for, as the runtime reports it.
#[derive(Clone, PartialEq, Eq)]
pub enum PreviewFailure {
    /// `invitation_invalid`.
    Invalid,
    /// `forbidden`: the server takes no new accounts.
    Closed,
    /// There is no such route: an older server.
    Unsupported,
    /// Nothing usable answered; why, in fixed words.
    Failed(String),
}

impl fmt::Debug for PreviewFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => f.write_str("Invalid"),
            Self::Closed => f.write_str("Closed"),
            Self::Unsupported => f.write_str("Unsupported"),
            Self::Failed(_) => f.write_str("Failed(..)"),
        }
    }
}
