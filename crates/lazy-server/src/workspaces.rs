//! Several workspaces per person (llm-docs/DECISIONS.md, D23): creating a workspace, and joining
//! one with an invitation as an existing account.
//!
//! Both take the person's advisory lock first ([`crate::profile::lock_person`]): a person creates
//! and joins one at a time, which makes the limit on workspaces created exact, makes joining
//! idempotent, and keeps the workspaces they belong to still while a profile change fans out.

use lazy_domain::workspace::{
    MAX_WORKSPACES_CREATED, canonical_invite_code, invite_code_is_wellformed,
};
use lazy_domain::{UserId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{AcceptInvitationRequest, CreateWorkspaceRequest, ErrorCode, JoinedWorkspace};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::app::AppState;
use crate::error::AppError;
use crate::{access, accounts, bootstrap, conversations, crypto, db, profile};

fn invitation_invalid() -> AppError {
    AppError::new(ErrorCode::InvitationInvalid, "that invitation is not valid")
}

fn id_taken() -> AppError {
    AppError::conflict("a workspace with that id already exists")
}

/// Creates a workspace with the id the client chose, with the caller as its owner and `#general`
/// as its default channel, and default settings. No event is appended: nobody else is in it yet,
/// and the response is the snapshot to subscribe after.
///
/// Repeating the request (same id, same name, same person, still a member) returns the workspace
/// unchanged; any other workspace with that id is `conflict`, whoever's it is, so the answer says
/// nothing about other people's workspaces. A person may create [`MAX_WORKSPACES_CREATED`]
/// (`limit_reached`, counted under the person's lock so it is exact), and the server may switch
/// creation off (`forbidden`).
pub async fn create(
    state: &AppState,
    actor: UserId,
    id: WorkspaceId,
    request: CreateWorkspaceRequest,
) -> Result<JoinedWorkspace, AppError> {
    if !state.options.open_workspace_creation {
        return Err(AppError::forbidden(
            "this server does not allow creating workspaces",
        ));
    }
    let mut tx = state.pool.begin().await?;
    profile::lock_person(&mut tx, actor).await?;
    let is_bot: bool = sqlx::query_scalar("SELECT is_bot FROM users WHERE id = $1")
        .bind(actor.as_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(AppError::unauthenticated)?;
    if is_bot {
        return Err(AppError::forbidden("an app cannot create workspaces"));
    }

    let existing: Option<(Option<Uuid>, String)> =
        sqlx::query_as("SELECT created_by, name FROM workspaces WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    match existing {
        Some((created_by, name)) => {
            let theirs = created_by == Some(actor.as_uuid()) && name == request.name.as_str();
            // The creator who has since been deactivated has no workspace to be handed back.
            if !theirs || access::active_role(&mut tx, id, actor).await?.is_none() {
                return Err(id_taken());
            }
        }
        None => {
            let created: i64 =
                sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE created_by = $1")
                    .bind(actor.as_uuid())
                    .fetch_one(&mut *tx)
                    .await?;
            if usize::try_from(created).unwrap_or(usize::MAX) >= MAX_WORKSPACES_CREATED {
                return Err(AppError::new(
                    ErrorCode::LimitReached,
                    format!("a person can create at most {MAX_WORKSPACES_CREATED} workspaces"),
                ));
            }
            // The id is the client's to choose, so it may be anyone's: the insert, not the look
            // above, decides (another creation of it that has not committed yet makes this wait).
            db::insert_workspace(&mut tx, id, request.name.as_str(), Some(actor))
                .await?
                .ok_or_else(id_taken)?;
            add_member(&mut tx, id, actor, WorkspaceRole::Owner).await?;
            conversations::join_default_channel(&mut tx, id, actor).await?;
        }
    }
    tx.commit().await?;
    let workspace = bootstrap::joined_workspace(&state.pool, actor, id).await?;
    Ok(JoinedWorkspace { workspace })
}

async fn add_member(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    user: UserId,
    role: WorkspaceRole,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(workspace.as_uuid())
        .bind(user.as_uuid())
        .bind(db::role_to_db(role))
        .execute(conn)
        .await?;
    Ok(())
}

/// Joins the workspace an invitation code belongs to, as a member with its default channel, and
/// counts the use; announces `MemberJoined` and `MemberAdded` exactly as registration does.
///
/// A code that is of the wrong shape, unknown, revoked, expired or used up is the same
/// `invitation_invalid`. Someone who already belongs is handed the workspace with nothing used and
/// nothing announced; someone whose membership is deactivated is `forbidden`.
///
/// **Locking.** The use is counted by one guarded `UPDATE` of the invitation's row, so of two
/// people on a one-use code the second waits, finds it used up, and fails; the join and its events
/// are in the same transaction, so a failure after it gives the use back. The invitation's row is
/// taken *before* the workspace's row (the append at the end), the order registration with a code
/// uses, because the other order would deadlock against it.
pub async fn accept_invitation(
    state: &AppState,
    actor: UserId,
    request: AcceptInvitationRequest,
) -> Result<JoinedWorkspace, AppError> {
    let code = canonical_invite_code(request.code.expose());
    if !invite_code_is_wellformed(&code) {
        return Err(invitation_invalid());
    }
    let hash = crypto::invite_code_hash(&code);

    let mut tx = state.pool.begin().await?;
    profile::lock_person(&mut tx, actor).await?;
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM workspace_invitations WHERE code_hash = $1")
            .bind(&hash[..])
            .fetch_optional(&mut *tx)
            .await?;
    let workspace = WorkspaceId::from_uuid(found.ok_or_else(invitation_invalid)?);

    // Whatever state the membership is in; the person's lock keeps it from changing under this by
    // another request of theirs.
    let membership: Option<bool> = sqlx::query_scalar(
        "SELECT deactivated_at IS NOT NULL FROM workspace_members \
         WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(actor.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    match membership {
        Some(false) => {
            drop(tx);
            let workspace = bootstrap::joined_workspace(&state.pool, actor, workspace).await?;
            return Ok(JoinedWorkspace { workspace });
        }
        Some(true) => {
            return Err(AppError::forbidden(
                "your membership of that workspace is deactivated",
            ));
        }
        None => {}
    }

    let used: Option<Uuid> = sqlx::query_scalar(
        "UPDATE workspace_invitations SET uses = uses + 1 \
         WHERE code_hash = $1 AND revoked_at IS NULL AND expires_at > now() \
           AND (max_uses IS NULL OR uses < max_uses) RETURNING id",
    )
    .bind(&hash[..])
    .fetch_optional(&mut *tx)
    .await?;
    used.ok_or_else(invitation_invalid)?;

    add_member(&mut tx, workspace, actor, WorkspaceRole::Member).await?;
    let default_channel = conversations::join_default_channel(&mut tx, workspace, actor).await?;
    let user = {
        let row: Option<(String, String, bool, i32)> = sqlx::query_as(
            "SELECT username, display_name, is_bot, avatar_version FROM users WHERE id = $1",
        )
        .bind(actor.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let (username, display_name, is_bot, avatar_version) =
            row.ok_or_else(AppError::unauthenticated)?;
        if is_bot {
            return Err(AppError::forbidden("an app cannot join a workspace"));
        }
        db::user_from_db(
            actor.as_uuid(),
            &username,
            &display_name,
            is_bot,
            avatar_version,
        )?
    };
    accounts::announce_member(&mut tx, workspace, user, default_channel).await?;
    tx.commit().await?;

    let workspace = bootstrap::joined_workspace(&state.pool, actor, workspace).await?;
    Ok(JoinedWorkspace { workspace })
}
