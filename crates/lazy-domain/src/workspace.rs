//! Workspace administration rules: who may change whose role or deactivate whom, the settings a
//! workspace has, and the limits on invitations. Pure, so the server enforces them against facts
//! it loads under lock and clients use them only to decide what to offer (D22).

use serde::{Deserialize, Serialize};

use crate::WorkspaceRole;

/// Who may create an account that joins a workspace by registering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationPolicy {
    /// Anyone who can reach the server may register and joins the default workspace.
    #[default]
    Open,
    /// Joining needs an invitation.
    InviteOnly,
}

/// Who may create channels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelCreationPolicy {
    #[default]
    Everyone,
    AdminsOnly,
}

impl ChannelCreationPolicy {
    /// Whether a person with `role` may create a channel.
    pub fn allows(self, role: WorkspaceRole) -> bool {
        match self {
            Self::Everyone => true,
            Self::AdminsOnly => role.can_manage_workspace(),
        }
    }
}

/// A workspace's settings other than its name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSettings {
    #[serde(default)]
    pub registration: RegistrationPolicy,
    #[serde(default)]
    pub channel_creation: ChannelCreationPolicy,
}

/// Why a role change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RoleChangeError {
    #[error("you cannot change this person's role")]
    NotAllowed,
    #[error("a workspace needs at least one owner")]
    LastOwner,
}

/// Whether `actor` may set `target`'s role to `new_role`. `other_owners` counts the active
/// owners besides `target`.
///
/// Members change nothing. Owners change anyone's role to anything, including their own, as long
/// as an owner remains. Admins move members and admins between those two roles; they never
/// touch an owner or make one. Setting the role someone already has is allowed to whoever could
/// change it.
pub fn check_role_change(
    actor: WorkspaceRole,
    target: WorkspaceRole,
    new_role: WorkspaceRole,
    other_owners: usize,
) -> Result<(), RoleChangeError> {
    match actor {
        WorkspaceRole::Member => Err(RoleChangeError::NotAllowed),
        WorkspaceRole::Admin => {
            if target == WorkspaceRole::Owner || new_role == WorkspaceRole::Owner {
                Err(RoleChangeError::NotAllowed)
            } else {
                Ok(())
            }
        }
        WorkspaceRole::Owner => {
            if target == WorkspaceRole::Owner
                && new_role != WorkspaceRole::Owner
                && other_owners == 0
            {
                Err(RoleChangeError::LastOwner)
            } else {
                Ok(())
            }
        }
    }
}

/// Why deactivating or reactivating someone was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemberActionError {
    #[error("you cannot do this to this person")]
    NotAllowed,
    #[error("you cannot deactivate yourself")]
    Yourself,
}

/// Whether `actor` may deactivate or reactivate `target` (a different person). Owners may act on
/// anyone else; admins on members; members on no one. A person cannot deactivate themself, so
/// the last owner can never be removed this way.
pub fn check_member_action(
    actor: WorkspaceRole,
    target: WorkspaceRole,
    is_self: bool,
) -> Result<(), MemberActionError> {
    if is_self {
        return Err(MemberActionError::Yourself);
    }
    match (actor, target) {
        (WorkspaceRole::Owner, _) => Ok(()),
        (WorkspaceRole::Admin, WorkspaceRole::Member) => Ok(()),
        _ => Err(MemberActionError::NotAllowed),
    }
}

/// The most workspaces one person can create.
pub const MAX_WORKSPACES_CREATED: usize = 10;

/// The largest picture a person may upload as their avatar, in bytes, as sent.
pub const MAX_AVATAR_UPLOAD_BYTES: usize = 5 * 1024 * 1024;
/// The side of the square the server stores an avatar at, in pixels.
pub const AVATAR_SIZE_PX: u32 = 256;

/// Shortest and longest an invitation may stay valid, and the default, in seconds.
pub const MIN_INVITE_TTL_SECS: u64 = 60 * 60;
pub const DEFAULT_INVITE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
pub const MAX_INVITE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// The most times one invitation can be used.
pub const MAX_INVITE_USES: u32 = 1_000;
/// The most invitations one workspace can have outstanding (not expired, revoked, or used up).
pub const MAX_ACTIVE_INVITATIONS: usize = 100;
/// Shortest and longest an invitation code may be.
pub const MIN_INVITE_CODE_LEN: usize = 20;
pub const MAX_INVITE_CODE_LEN: usize = 64;

const _: () = {
    assert!(MIN_INVITE_TTL_SECS <= DEFAULT_INVITE_TTL_SECS);
    assert!(DEFAULT_INVITE_TTL_SECS <= MAX_INVITE_TTL_SECS);
    assert!(MIN_INVITE_CODE_LEN <= MAX_INVITE_CODE_LEN);
};

/// Whether `code` has the shape of an invitation code: 20 to 64 ASCII letters, digits, `_`, or
/// `-`. A cheap check before touching storage; it says nothing about whether the code is valid.
pub fn invite_code_is_wellformed(code: &str) -> bool {
    (MIN_INVITE_CODE_LEN..=MAX_INVITE_CODE_LEN).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use WorkspaceRole::{Admin, Member, Owner};

    const ROLES: [WorkspaceRole; 3] = [Member, Admin, Owner];

    #[test]
    fn role_changes_follow_the_table() {
        // (actor, target, new, other owners) -> allowed
        for actor in ROLES {
            for target in ROLES {
                for new_role in ROLES {
                    for other_owners in 0..3 {
                        let got = check_role_change(actor, target, new_role, other_owners);
                        let want = match actor {
                            Member => Err(RoleChangeError::NotAllowed),
                            Admin if target == Owner || new_role == Owner => {
                                Err(RoleChangeError::NotAllowed)
                            }
                            Admin => Ok(()),
                            Owner if target == Owner && new_role != Owner && other_owners == 0 => {
                                Err(RoleChangeError::LastOwner)
                            }
                            Owner => Ok(()),
                        };
                        assert_eq!(
                            got, want,
                            "{actor:?} sets {target:?} to {new_role:?} with {other_owners} other owners"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_workspace_always_keeps_an_owner() {
        // Whatever an owner or admin tries on the only owner, the owner is still one.
        for actor in ROLES {
            for new_role in ROLES {
                if check_role_change(actor, Owner, new_role, 0).is_ok() {
                    assert_eq!(new_role, Owner, "{actor:?} demoted the last owner");
                }
            }
        }
    }

    #[test]
    fn admins_never_create_owners_or_touch_them() {
        assert_eq!(
            check_role_change(Admin, Member, Owner, 1),
            Err(RoleChangeError::NotAllowed)
        );
        assert_eq!(
            check_role_change(Admin, Owner, Member, 1),
            Err(RoleChangeError::NotAllowed)
        );
        assert_eq!(
            check_role_change(Admin, Owner, Owner, 1),
            Err(RoleChangeError::NotAllowed)
        );
        assert_eq!(check_role_change(Admin, Member, Admin, 0), Ok(()));
        assert_eq!(check_role_change(Admin, Admin, Member, 0), Ok(()));
    }

    #[test]
    fn deactivation_follows_the_table() {
        for actor in ROLES {
            for target in ROLES {
                assert_eq!(
                    check_member_action(actor, target, true),
                    Err(MemberActionError::Yourself)
                );
                let want = matches!((actor, target), (Owner, _) | (Admin, Member));
                assert_eq!(
                    check_member_action(actor, target, false).is_ok(),
                    want,
                    "{actor:?} on {target:?}"
                );
            }
        }
    }

    #[test]
    fn channel_creation_policy_decides_by_role() {
        for role in ROLES {
            assert!(ChannelCreationPolicy::Everyone.allows(role));
            assert_eq!(
                ChannelCreationPolicy::AdminsOnly.allows(role),
                role != Member
            );
        }
        assert_eq!(
            ChannelCreationPolicy::default(),
            ChannelCreationPolicy::Everyone
        );
        assert_eq!(RegistrationPolicy::default(), RegistrationPolicy::Open);
    }

    #[test]
    fn settings_read_with_missing_fields_and_use_snake_case() {
        let settings: WorkspaceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, WorkspaceSettings::default());
        let settings: WorkspaceSettings = serde_json::from_str(
            r#"{"registration":"invite_only","channel_creation":"admins_only"}"#,
        )
        .unwrap();
        assert_eq!(settings.registration, RegistrationPolicy::InviteOnly);
        assert_eq!(settings.channel_creation, ChannelCreationPolicy::AdminsOnly);
    }

    #[test]
    fn invite_codes_are_checked_for_shape() {
        assert!(invite_code_is_wellformed(&"a".repeat(MIN_INVITE_CODE_LEN)));
        assert!(invite_code_is_wellformed(&"Z9_-".repeat(16)));
        assert!(!invite_code_is_wellformed(
            &"a".repeat(MIN_INVITE_CODE_LEN - 1)
        ));
        assert!(!invite_code_is_wellformed(
            &"a".repeat(MAX_INVITE_CODE_LEN + 1)
        ));
        assert!(!invite_code_is_wellformed(&format!("{} ", "a".repeat(30))));
        assert!(!invite_code_is_wellformed(&format!("{}é", "a".repeat(30))));
        assert!(!invite_code_is_wellformed(""));
    }

    #[test]
    fn workspace_names_are_trimmed_and_bounded() {
        use crate::WorkspaceName;
        assert_eq!(WorkspaceName::parse("  Acme  ").unwrap().as_str(), "Acme");
        assert!(WorkspaceName::parse("   ").is_err());
        assert!(WorkspaceName::parse("bad\nname").is_err());
        assert!(WorkspaceName::parse(&"x".repeat(WorkspaceName::MAX_LEN)).is_ok());
        assert!(WorkspaceName::parse(&"x".repeat(WorkspaceName::MAX_LEN + 1)).is_err());
    }

    proptest::proptest! {
        #[test]
        fn an_admin_or_member_never_changes_an_owner(
            actor in 0usize..2,
            new_role in 0usize..3,
            others in 0usize..4,
        ) {
            let actor = ROLES[actor];
            assert!(check_role_change(actor, Owner, ROLES[new_role], others).is_err());
        }
    }
}
