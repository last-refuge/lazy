//! Pure helpers for the administration dialogs (D22): the choices an invitation is made with, the
//! roles a person may be given, and the words for where an invitation stands.
//!
//! The rules come from `lazy_domain::workspace`; the client uses them only to decide what to
//! offer, the server decides. Everything takes the current time and the person's time zone as
//! arguments, so each rule is a unit test.

use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_domain::workspace::{
    MAX_INVITE_TTL_SECS, MAX_INVITE_USES, MIN_INVITE_TTL_SECS, check_member_action,
    check_role_change,
};
use lazy_domain::{Timestamp, WorkspaceRole};
use lazy_protocol::{Invitation, InvitationState};

use crate::notification_form::moment_label;
use crate::time_fmt::{ago_label, to_zoned};

/// How long a new invitation stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Hour,
    Day,
    Week,
    Month,
}

impl Expiry {
    pub const ALL: [Expiry; 4] = [Expiry::Hour, Expiry::Day, Expiry::Week, Expiry::Month];
    /// What the form starts with: the server's own default of 7 days.
    pub const DEFAULT: Expiry = Expiry::Week;

    pub fn label(self) -> &'static str {
        match self {
            Self::Hour => "1 hour",
            Self::Day => "1 day",
            Self::Week => "7 days",
            Self::Month => "30 days",
        }
    }

    pub fn secs(self) -> u64 {
        match self {
            Self::Hour => 60 * 60,
            Self::Day => 24 * 60 * 60,
            Self::Week => 7 * 24 * 60 * 60,
            Self::Month => 30 * 24 * 60 * 60,
        }
    }
}

/// How many times a new invitation can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uses {
    Unlimited,
    One,
    Five,
    TwentyFive,
    Hundred,
}

impl Uses {
    pub const ALL: [Uses; 5] = [
        Uses::Unlimited,
        Uses::One,
        Uses::Five,
        Uses::TwentyFive,
        Uses::Hundred,
    ];
    pub const DEFAULT: Uses = Uses::Unlimited;

    pub fn label(self) -> &'static str {
        match self {
            Self::Unlimited => "Unlimited",
            Self::One => "1 use",
            Self::Five => "5 uses",
            Self::TwentyFive => "25 uses",
            Self::Hundred => "100 uses",
        }
    }

    pub fn max(self) -> Option<u32> {
        match self {
            Self::Unlimited => None,
            Self::One => Some(1),
            Self::Five => Some(5),
            Self::TwentyFive => Some(25),
            Self::Hundred => Some(100),
        }
    }
}

/// The word for a role.
pub fn role_name(role: WorkspaceRole) -> &'static str {
    match role {
        WorkspaceRole::Owner => "Owner",
        WorkspaceRole::Admin => "Admin",
        WorkspaceRole::Member => "Member",
    }
}

/// What a role lets a person do, in a line under the control that sets it.
pub fn role_detail(role: WorkspaceRole) -> &'static str {
    match role {
        WorkspaceRole::Owner => "Everything an admin can, and makes other owners.",
        WorkspaceRole::Admin => "Manages the workspace, its invitations and its members.",
        WorkspaceRole::Member => "Takes part in channels and conversations.",
    }
}

/// The roles `actor` may give `target` (who has `current` now), best first. Empty when they may
/// not change this person's role at all. The server alone knows whether another owner remains, so
/// an owner is offered the choice of demoting an owner and the server's refusal ("a workspace
/// needs at least one owner") is what says no.
pub fn role_choices(actor: WorkspaceRole, current: WorkspaceRole) -> Vec<WorkspaceRole> {
    [
        WorkspaceRole::Owner,
        WorkspaceRole::Admin,
        WorkspaceRole::Member,
    ]
    .into_iter()
    .filter(|new_role| check_role_change(actor, current, *new_role, 1).is_ok())
    .collect()
}

/// Whether `actor` may deactivate or reactivate `target`, who has `target_role`.
pub fn may_deactivate(actor: WorkspaceRole, target_role: WorkspaceRole, is_self: bool) -> bool {
    check_member_action(actor, target_role, is_self).is_ok()
}

/// The badge for where an invitation stands.
pub fn state_label(state: InvitationState) -> &'static str {
    match state {
        InvitationState::Active => "Active",
        InvitationState::Expired => "Expired",
        InvitationState::Revoked => "Revoked",
        InvitationState::UsedUp => "Used up",
    }
}

/// How many times it was used: `2 of 5 uses`, or `3 uses` when there is no limit.
pub fn uses_text(uses: u32, max: Option<u32>) -> String {
    let noun = |n: u32| if n == 1 { "use" } else { "uses" };
    match max {
        Some(max) => format!("{uses} of {max} {}", noun(max)),
        None => format!("{uses} {}", noun(uses)),
    }
}

/// How many times a new invitation can be used: `Up to 5 uses`, `One use`, `No limit on uses`.
pub fn capacity_text(max: Option<u32>) -> String {
    match max {
        Some(1) => "One use".to_owned(),
        Some(max) => format!("Up to {max} uses"),
        None => "No limit on uses".to_owned(),
    }
}

/// When it runs out (or ran out), in the person's time: `Expires tomorrow 9:00 AM` for an active
/// one, `Expired yesterday` for one that lapsed. A revoked or used-up one says nothing about
/// time: its state is the news.
pub fn expiry_text(invitation: &Invitation, now: Timestamp, zone: &TimeZone) -> Option<String> {
    match invitation.state {
        InvitationState::Active => Some(format!(
            "Expires {}",
            moment_label(invitation.expires_at, now, zone)
        )),
        InvitationState::Expired => {
            let (then, today): (Zoned, Zoned) =
                (to_zoned(invitation.expires_at, zone), to_zoned(now, zone));
            Some(format!("Expired {}", ago_label(&then, &today)))
        }
        InvitationState::Revoked | InvitationState::UsedUp => None,
    }
}

/// Whether a state can still be revoked: only an active invitation can be taken back.
pub fn can_revoke(state: InvitationState) -> bool {
    state == InvitationState::Active
}

/// Bounds the form's choices must stay inside, for the test that guards them.
pub const TTL_BOUNDS: (u64, u64) = (MIN_INVITE_TTL_SECS, MAX_INVITE_TTL_SECS);
pub const USES_BOUND: u32 = MAX_INVITE_USES;

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use lazy_domain::{InvitationId, UserId, WorkspaceId};

    use super::*;
    use WorkspaceRole::{Admin, Member, Owner};

    fn at(year: i16, month: i8, day: i8, hour: i8, zone: &TimeZone) -> Timestamp {
        let moment = date(year, month, day)
            .at(hour, 0, 0, 0)
            .to_zoned(zone.clone())
            .unwrap();
        Timestamp::from(
            time::OffsetDateTime::from_unix_timestamp(moment.timestamp().as_second()).unwrap(),
        )
    }

    fn invitation(state: InvitationState, expires_at: Timestamp) -> Invitation {
        Invitation {
            id: InvitationId::generate(),
            workspace_id: WorkspaceId::generate(),
            created_by: UserId::generate(),
            created_at: expires_at,
            expires_at,
            max_uses: Some(5),
            uses: 2,
            state,
        }
    }

    #[test]
    fn every_choice_is_one_the_server_accepts() {
        for expiry in Expiry::ALL {
            assert!(
                (TTL_BOUNDS.0..=TTL_BOUNDS.1).contains(&expiry.secs()),
                "{expiry:?}"
            );
        }
        assert_eq!(
            Expiry::DEFAULT.secs(),
            lazy_domain::workspace::DEFAULT_INVITE_TTL_SECS
        );
        for uses in Uses::ALL {
            assert!(
                uses.max().is_none_or(|max| (1..=USES_BOUND).contains(&max)),
                "{uses:?}"
            );
        }
        assert_eq!(Uses::DEFAULT.max(), None);
        let labels: Vec<_> = Expiry::ALL.iter().map(|e| e.label()).collect();
        assert_eq!(labels, ["1 hour", "1 day", "7 days", "30 days"]);
    }

    #[test]
    fn a_person_is_offered_only_the_roles_they_may_give() {
        assert_eq!(role_choices(Owner, Member), vec![Owner, Admin, Member]);
        assert_eq!(role_choices(Owner, Owner), vec![Owner, Admin, Member]);
        assert_eq!(role_choices(Admin, Member), vec![Admin, Member]);
        assert_eq!(role_choices(Admin, Admin), vec![Admin, Member]);
        assert!(
            role_choices(Admin, Owner).is_empty(),
            "admins never touch an owner"
        );
        assert!(role_choices(Member, Member).is_empty());
        assert!(role_choices(Member, Admin).is_empty());
    }

    #[test]
    fn deactivating_follows_the_server_table() {
        assert!(may_deactivate(Owner, Admin, false));
        assert!(may_deactivate(Owner, Owner, false));
        assert!(may_deactivate(Admin, Member, false));
        assert!(!may_deactivate(Admin, Admin, false));
        assert!(!may_deactivate(Admin, Owner, false));
        assert!(!may_deactivate(Member, Member, false));
        assert!(
            !may_deactivate(Owner, Owner, true),
            "nobody deactivates themself"
        );
    }

    #[test]
    fn uses_read_naturally() {
        assert_eq!(uses_text(0, Some(1)), "0 of 1 use");
        assert_eq!(uses_text(2, Some(5)), "2 of 5 uses");
        assert_eq!(uses_text(1, None), "1 use");
        assert_eq!(uses_text(7, None), "7 uses");
    }

    #[test]
    fn capacity_reads_naturally() {
        assert_eq!(capacity_text(Some(1)), "One use");
        assert_eq!(capacity_text(Some(25)), "Up to 25 uses");
        assert_eq!(capacity_text(None), "No limit on uses");
    }

    #[test]
    fn an_invitation_says_when_it_expires_or_that_it_did() {
        let zone = TimeZone::get("America/Phoenix").unwrap();
        let now = at(2026, 10, 29, 14, &zone);
        let tomorrow = invitation(InvitationState::Active, at(2026, 10, 30, 9, &zone));
        assert_eq!(
            expiry_text(&tomorrow, now, &zone).as_deref(),
            Some("Expires tomorrow 9:00 AM")
        );
        let lapsed = invitation(InvitationState::Expired, at(2026, 10, 28, 9, &zone));
        assert_eq!(
            expiry_text(&lapsed, now, &zone).as_deref(),
            Some("Expired yesterday")
        );
        assert_eq!(
            expiry_text(&invitation(InvitationState::Revoked, now), now, &zone),
            None
        );
        assert_eq!(
            expiry_text(&invitation(InvitationState::UsedUp, now), now, &zone),
            None
        );
    }

    #[test]
    fn only_an_active_invitation_can_be_revoked() {
        assert!(can_revoke(InvitationState::Active));
        for state in [
            InvitationState::Expired,
            InvitationState::Revoked,
            InvitationState::UsedUp,
        ] {
            assert!(!can_revoke(state));
        }
        assert_eq!(state_label(InvitationState::UsedUp), "Used up");
    }
}
