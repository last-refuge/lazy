//! Who counts as a member of a workspace (llm-docs/DECISIONS.md, D22): **one definition, used
//! everywhere**.
//!
//! A membership whose `deactivated_at` is set gives no access. Every place that lets a request,
//! a search, an attachment, an emoji, a group, an inbox, a saved list, a people directory, a
//! realtime subscription or a live delivery through because the person is in the workspace goes
//! through [`active_member!`] (in SQL) or [`active_role`] / [`require_member`] (in code), so a
//! deactivated person gets exactly what someone who is not a member gets. The session stays valid:
//! the account is theirs and may belong to other workspaces.
//!
//! What does *not* use it, on purpose: the lists that show deactivated people flagged (the
//! bootstrap's `members`, a profile, the people directory for an admin who asks), the invitation
//! and role code that must find a membership whatever its state, and "is this a person of the
//! workspace" validation (a mention of someone deactivated is plain text, not an error).

use lazy_domain::{UserId, WorkspaceId, WorkspaceRole};
use sqlx::PgConnection;

use crate::db;
use crate::error::AppError;

/// The SQL condition "this `workspace_members` row (alias given) is an active membership".
macro_rules! active_member {
    ($alias:literal) => {
        concat!($alias, ".deactivated_at IS NULL")
    };
}
pub(crate) use active_member;

/// The person's role in the workspace, or `None` if they have no active membership of it (they are
/// not in it, or are deactivated).
pub(crate) async fn active_role(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<Option<WorkspaceRole>, AppError> {
    let role: Option<String> = sqlx::query_scalar(concat!(
        "SELECT wm.role FROM workspace_members wm \
         WHERE wm.workspace_id = $1 AND wm.user_id = $2 AND ",
        active_member!("wm")
    ))
    .bind(workspace.as_uuid())
    .bind(user.as_uuid())
    .fetch_optional(conn)
    .await?;
    role.as_deref().map(db::role_from_db).transpose()
}

/// [`active_role`], or `not_found` ("workspace not found": the answer for a workspace that does not
/// exist, so nothing is revealed).
pub(crate) async fn require_member(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<WorkspaceRole, AppError> {
    active_role(conn, workspace, user)
        .await?
        .ok_or_else(|| AppError::not_found("workspace not found"))
}
