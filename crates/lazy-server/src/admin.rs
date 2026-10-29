//! Workspace administration (llm-docs/DECISIONS.md, D22): the workspace's name and settings, roles,
//! deactivation, and invitations.
//!
//! **Locking.** Every change that must hold under races — a role change, a deactivation, a settings
//! change, creating an invitation (the limit of active ones), and using one — takes the workspace
//! row `FOR UPDATE`, the row the event log's position counter also locks (D3), and reads the roles
//! it decides on *after* taking it. So two owners demoting each other at once leave one owner (the
//! second finds themself a member), and two registrations on a one-use invitation leave one winner.
//! Changes are announced only when something changed, in the transaction that made them.
//!
//! The rules (who may do what) are `lazy_domain::workspace`; the server applies them to the roles
//! it reads under the lock, and clients use them only to decide what to offer.

use lazy_domain::workspace::{
    DEFAULT_INVITE_TTL_SECS, MAX_ACTIVE_INVITATIONS, MAX_INVITE_TTL_SECS, MAX_INVITE_USES,
    MIN_INVITE_TTL_SECS, MemberActionError, RoleChangeError, WorkspaceSettings,
    check_member_action, check_role_change,
};
use lazy_domain::{InvitationId, Timestamp, UserId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{
    CreateInvitationRequest, CreatedInvitation, ErrorCode, Event, Invitation, InvitationList,
    InvitationState, Person, UpdateWorkspaceRequest, Workspace,
};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::{access, crypto, db, discovery};

/// The most invitations one listing returns (the newest).
const MAX_LISTED_INVITATIONS: i64 = 1_000;

fn forbidden(message: impl Into<String>) -> AppError {
    AppError::forbidden(message)
}

/// Takes the workspace row (the lock the event log's positions use). `not_found` if there is none.
pub(crate) async fn lock_workspace(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), AppError> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.as_uuid())
            .fetch_optional(conn)
            .await?;
    found
        .map(|_| ())
        .ok_or_else(|| AppError::not_found("workspace not found"))
}

/// The workspace as clients see it: id, name, settings.
pub(crate) async fn load_workspace(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Workspace, AppError> {
    let row: Option<(String, Json<WorkspaceSettings>)> =
        sqlx::query_as("SELECT name, settings FROM workspaces WHERE id = $1")
            .bind(workspace.as_uuid())
            .fetch_optional(conn)
            .await?;
    let (name, settings) = row.ok_or_else(|| AppError::not_found("workspace not found"))?;
    Ok(Workspace {
        id: workspace,
        name,
        settings: settings.0,
    })
}

/// An active member who may manage the workspace (an admin or an owner): `not_found` for anyone
/// else outside it, `forbidden` for a member.
async fn require_manager(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    actor: UserId,
) -> Result<WorkspaceRole, AppError> {
    let role = access::require_member(conn, workspace, actor).await?;
    if role.can_manage_workspace() {
        Ok(role)
    } else {
        Err(forbidden("only admins and owners can do this"))
    }
}

// --- The workspace ---------------------------------------------------------------------------

/// Changes the workspace's name and/or settings. Admins and owners only. Absent fields are
/// unchanged; asking for what already is commits nothing and announces nothing; a change appends
/// `WorkspaceUpdated` to the whole workspace in the same transaction.
pub async fn update_workspace(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    request: UpdateWorkspaceRequest,
) -> Result<Workspace, AppError> {
    let mut tx = pool.begin().await?;
    lock_workspace(&mut tx, workspace).await?;
    require_manager(&mut tx, workspace, actor).await?;
    let current = load_workspace(&mut tx, workspace).await?;
    let name = request
        .name
        .as_ref()
        .map_or_else(|| current.name.clone(), |n| n.as_str().to_owned());
    let settings = request.settings.unwrap_or(current.settings);
    if name == current.name && settings == current.settings {
        return Ok(current);
    }
    sqlx::query("UPDATE workspaces SET name = $2, settings = $3 WHERE id = $1")
        .bind(workspace.as_uuid())
        .bind(&name)
        .bind(Json(settings))
        .execute(&mut *tx)
        .await?;
    let updated = Workspace {
        id: workspace,
        name,
        settings,
    };
    events::append_all(
        &mut tx,
        workspace,
        vec![PendingEvent {
            kind: "workspace_updated",
            conversation_id: None,
            operation_id: None,
            audience: Audience::Workspace,
            event: Event::WorkspaceUpdated {
                workspace: updated.clone(),
            },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(updated)
}

// --- Roles and deactivation ------------------------------------------------------------------

struct Target {
    role: WorkspaceRole,
    is_bot: bool,
}

/// The target's membership, whatever its state (a deactivated person can be given a role and
/// reactivated), locked. `not_found` if they are not in the workspace.
async fn lock_target(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    target: UserId,
) -> Result<Target, AppError> {
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT wm.role, u.is_bot FROM workspace_members wm JOIN users u ON u.id = wm.user_id \
         WHERE wm.workspace_id = $1 AND wm.user_id = $2 FOR UPDATE OF wm",
    )
    .bind(workspace.as_uuid())
    .bind(target.as_uuid())
    .fetch_optional(conn)
    .await?;
    let (role, is_bot) = row.ok_or_else(|| AppError::not_found("person not found"))?;
    Ok(Target {
        role: db::role_from_db(&role)?,
        is_bot,
    })
}

/// Sets a person's role. `check_role_change` decides (`forbidden`; `conflict` for the last owner),
/// against the roles read under the workspace lock; the active owners besides the target are
/// counted, so a workspace is never left with none. Setting the role someone has commits nothing.
/// An app (a webhook's bot) has no role to change (`invalid_request`).
pub async fn set_role(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    target: UserId,
    new_role: WorkspaceRole,
) -> Result<Person, AppError> {
    let mut tx = pool.begin().await?;
    lock_workspace(&mut tx, workspace).await?;
    let actor_role = access::require_member(&mut tx, workspace, actor).await?;
    let found = lock_target(&mut tx, workspace, target).await?;
    if found.is_bot {
        return Err(AppError::invalid("an app has no role to change"));
    }
    let other_owners: i64 = sqlx::query_scalar(concat!(
        "SELECT count(*) FROM workspace_members wm \
         WHERE wm.workspace_id = $1 AND wm.role = 'owner' AND wm.user_id <> $2 AND ",
        access::active_member!("wm")
    ))
    .bind(workspace.as_uuid())
    .bind(target.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    check_role_change(
        actor_role,
        found.role,
        new_role,
        usize::try_from(other_owners).unwrap_or(usize::MAX),
    )
    .map_err(|error| match error {
        RoleChangeError::NotAllowed => forbidden(error.to_string()),
        RoleChangeError::LastOwner => AppError::new(ErrorCode::Conflict, error.to_string()),
    })?;
    if found.role != new_role {
        sqlx::query(
            "UPDATE workspace_members SET role = $3 WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(target.as_uuid())
        .bind(db::role_to_db(new_role))
        .execute(&mut *tx)
        .await?;
        events::append_all(
            &mut tx,
            workspace,
            vec![PendingEvent {
                kind: "member_role_changed",
                conversation_id: None,
                operation_id: None,
                audience: Audience::Workspace,
                event: Event::MemberRoleChanged {
                    user_id: target,
                    role: new_role,
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    discovery::person(pool, actor, workspace, target).await
}

/// Deactivates or reactivates a person's membership. `check_member_action` decides (`forbidden`;
/// nobody deactivates themself, so the last owner stays). Setting the state someone is in commits
/// nothing. A change appends `MemberDeactivated` to the workspace; the person's own subscription
/// ends when that batch reaches it (their membership no longer counts), without being told. Their
/// session, messages, role and conversation memberships are untouched. An app cannot be
/// deactivated (`invalid_request`; its webhook is revoked instead).
pub async fn set_deactivated(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    target: UserId,
    deactivated: bool,
) -> Result<Person, AppError> {
    let mut tx = pool.begin().await?;
    lock_workspace(&mut tx, workspace).await?;
    let actor_role = access::require_member(&mut tx, workspace, actor).await?;
    let found = lock_target(&mut tx, workspace, target).await?;
    if found.is_bot {
        return Err(AppError::invalid(
            "an app cannot be deactivated; revoke its webhook instead",
        ));
    }
    check_member_action(actor_role, found.role, actor == target).map_err(|error| match error {
        MemberActionError::NotAllowed | MemberActionError::Yourself => forbidden(error.to_string()),
    })?;
    let changed = sqlx::query(
        "UPDATE workspace_members \
         SET deactivated_at = CASE WHEN $3 THEN now() ELSE NULL END \
         WHERE workspace_id = $1 AND user_id = $2 AND (deactivated_at IS NOT NULL) <> $3",
    )
    .bind(workspace.as_uuid())
    .bind(target.as_uuid())
    .bind(deactivated)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if changed {
        events::append_all(
            &mut tx,
            workspace,
            vec![PendingEvent {
                kind: "member_deactivated",
                conversation_id: None,
                operation_id: None,
                audience: Audience::Workspace,
                event: Event::MemberDeactivated {
                    user_id: target,
                    deactivated,
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    discovery::person(pool, actor, workspace, target).await
}

// --- Invitations -----------------------------------------------------------------------------

type InvitationRow = (
    Uuid,
    Uuid,
    Uuid,
    OffsetDateTime,
    OffsetDateTime,
    Option<i32>,
    i32,
    String,
);

/// The columns of an invitation, with its state worked out by the database's clock: revoked first,
/// then used up, then expired, else active.
macro_rules! invitation_columns {
    () => {
        "i.id, i.workspace_id, i.created_by, i.created_at, i.expires_at, i.max_uses, i.uses, \
         CASE WHEN i.revoked_at IS NOT NULL THEN 'revoked' \
              WHEN i.max_uses IS NOT NULL AND i.uses >= i.max_uses THEN 'used_up' \
              WHEN i.expires_at <= now() THEN 'expired' \
              ELSE 'active' END AS state"
    };
}

fn invitation_from_row(row: InvitationRow) -> Result<Invitation, AppError> {
    let (id, workspace, created_by, created_at, expires_at, max_uses, uses, state) = row;
    Ok(Invitation {
        id: InvitationId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace),
        created_by: UserId::from_uuid(created_by),
        created_at: Timestamp::from(created_at),
        expires_at: Timestamp::from(expires_at),
        max_uses: max_uses.map(|n| u32::try_from(n).unwrap_or(0)),
        uses: u32::try_from(uses).unwrap_or(0),
        state: match state.as_str() {
            "active" => InvitationState::Active,
            "expired" => InvitationState::Expired,
            "revoked" => InvitationState::Revoked,
            "used_up" => InvitationState::UsedUp,
            other => {
                return Err(AppError::internal(format!(
                    "unknown invitation state {other:?}"
                )));
            }
        },
    })
}

/// Makes an invitation. Admins and owners only. `expires_in_secs` is between one hour and thirty
/// days (default seven days) and `max_uses` between 1 and 1,000 (`invalid_request`); a workspace
/// may have 100 active ones (`limit_reached`, counted under the workspace lock so it is exact). The
/// code is made from the system's randomness, only its hash is stored, and the response is the one
/// time it is shown.
pub async fn create_invitation(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    request: CreateInvitationRequest,
) -> Result<CreatedInvitation, AppError> {
    let ttl = request.expires_in_secs.unwrap_or(DEFAULT_INVITE_TTL_SECS);
    if !(MIN_INVITE_TTL_SECS..=MAX_INVITE_TTL_SECS).contains(&ttl) {
        return Err(AppError::invalid(
            "an invitation can be valid for between 1 hour and 30 days",
        ));
    }
    if let Some(uses) = request.max_uses
        && !(1..=MAX_INVITE_USES).contains(&uses)
    {
        return Err(AppError::invalid(format!(
            "an invitation can be used between 1 and {MAX_INVITE_USES} times"
        )));
    }
    let mut tx = pool.begin().await?;
    lock_workspace(&mut tx, workspace).await?;
    require_manager(&mut tx, workspace, actor).await?;
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_invitations i WHERE i.workspace_id = $1 \
         AND i.revoked_at IS NULL AND i.expires_at > now() \
         AND (i.max_uses IS NULL OR i.uses < i.max_uses)",
    )
    .bind(workspace.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if usize::try_from(active).unwrap_or(usize::MAX) >= MAX_ACTIVE_INVITATIONS {
        return Err(AppError::new(
            ErrorCode::LimitReached,
            format!("a workspace can have at most {MAX_ACTIVE_INVITATIONS} active invitations"),
        ));
    }
    let (code, hash) = crypto::new_invite_code()?;
    let id = InvitationId::generate();
    let row: InvitationRow = sqlx::query_as(concat!(
        "WITH i AS ( \
             INSERT INTO workspace_invitations (id, workspace_id, code_hash, created_by, expires_at, max_uses) \
             VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5), $6) RETURNING *) \
         SELECT ",
        invitation_columns!(),
        " FROM i"
    ))
    .bind(id.as_uuid())
    .bind(workspace.as_uuid())
    .bind(&hash[..])
    .bind(actor.as_uuid())
    .bind(ttl as f64)
    .bind(request.max_uses.map(|n| n as i32))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CreatedInvitation {
        invitation: invitation_from_row(row)?,
        code,
    })
}

/// The workspace's invitations that were made in the last 30 days or are still active, newest
/// first (at most 1,000). Admins and owners only.
pub async fn list_invitations(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<InvitationList, AppError> {
    let mut conn = pool.acquire().await?;
    require_manager(&mut conn, workspace, actor).await?;
    let rows: Vec<InvitationRow> = sqlx::query_as(concat!(
        "SELECT ",
        invitation_columns!(),
        " FROM workspace_invitations i WHERE i.workspace_id = $1 \
           AND (i.created_at > now() - interval '30 days' \
                OR (i.revoked_at IS NULL AND i.expires_at > now() \
                    AND (i.max_uses IS NULL OR i.uses < i.max_uses))) \
         ORDER BY i.created_at DESC, i.id DESC LIMIT $2"
    ))
    .bind(workspace.as_uuid())
    .bind(MAX_LISTED_INVITATIONS)
    .fetch_all(&mut *conn)
    .await?;
    Ok(InvitationList {
        invitations: rows
            .into_iter()
            .map(invitation_from_row)
            .collect::<Result<_, _>>()?,
    })
}

/// Revokes an invitation (admins and owners of its workspace). Revoking a revoked one returns it
/// unchanged. Someone outside the workspace, and an unknown id, get the same `not_found`.
pub async fn revoke_invitation(
    pool: &PgPool,
    actor: UserId,
    invitation: InvitationId,
) -> Result<Invitation, AppError> {
    let not_found = || AppError::not_found("invitation not found");
    let mut tx = pool.begin().await?;
    let workspace: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM workspace_invitations WHERE id = $1")
            .bind(invitation.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    let workspace = WorkspaceId::from_uuid(workspace.ok_or_else(not_found)?);
    match access::active_role(&mut tx, workspace, actor).await? {
        None => return Err(not_found()),
        Some(role) if !role.can_manage_workspace() => {
            return Err(forbidden("only admins and owners can revoke invitations"));
        }
        Some(_) => {}
    }
    let row: InvitationRow = sqlx::query_as(concat!(
        "WITH i AS ( \
             UPDATE workspace_invitations SET revoked_at = COALESCE(revoked_at, now()) \
             WHERE id = $1 RETURNING *) \
         SELECT ",
        invitation_columns!(),
        " FROM i"
    ))
    .bind(invitation.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    invitation_from_row(row)
}
