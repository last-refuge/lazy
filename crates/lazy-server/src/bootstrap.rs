//! The bootstrap snapshot that starts a client's synchronization.

use lazy_domain::workspace::WorkspaceSettings;
use lazy_domain::{EventSeq, UserId, WorkspaceId};
use lazy_protocol::{BootstrapResponse, PROTOCOL_VERSION, Workspace, WorkspaceBootstrap};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::conversations;
use crate::db;
use crate::discovery;
use crate::emoji;
use crate::error::AppError;
use crate::groups;
use crate::notifications;
use crate::threads;

type Membership = (
    Uuid,
    String,
    i64,
    String,
    sqlx::types::Json<WorkspaceSettings>,
);

/// The person's active memberships (`$1`), with the workspace's name, head, role and settings; the
/// tail is appended by the caller.
macro_rules! membership_select {
    () => {
        membership_select!("")
    };
    ($extra:literal) => {
        concat!(
            "SELECT w.id, w.name, w.event_head, wm.role, w.settings \
             FROM workspace_members wm JOIN workspaces w ON w.id = wm.workspace_id \
             WHERE wm.user_id = $1 AND ",
            crate::access::active_member!("wm"),
            $extra
        )
    };
}
/// Returns the caller's workspaces with the conversations they belong to (and their favorite and
/// read state for each), the members, and the event-log position the snapshot corresponds to.
///
/// Everything is read in one `REPEATABLE READ` read-only transaction. A writer inserts an event
/// and bumps `event_head` in a single transaction, so the snapshot includes exactly the effects
/// of events up to `head`: a client that then subscribes `after = head` neither misses nor
/// double-applies any event.
pub async fn bootstrap(pool: &PgPool, user_id: UserId) -> Result<BootstrapResponse, AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;

    let user = {
        let row: Option<(String, String, bool, i32)> = sqlx::query_as(
            "SELECT username, display_name, is_bot, avatar_version FROM users WHERE id = $1",
        )
        .bind(user_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let (username, display_name, is_bot, avatar_version) =
            row.ok_or_else(AppError::unauthenticated)?;
        db::user_from_db(
            user_id.as_uuid(),
            &username,
            &display_name,
            is_bot,
            avatar_version,
        )?
    };

    // A workspace the person is deactivated in is not theirs: it leaves the bootstrap.
    let memberships: Vec<Membership> = sqlx::query_as(concat!(
        membership_select!(),
        " ORDER BY w.created_at, w.id"
    ))
    .bind(user_id.as_uuid())
    .fetch_all(&mut *tx)
    .await?;

    let mut workspaces = Vec::with_capacity(memberships.len());
    for membership in memberships {
        workspaces.push(workspace_bootstrap(&mut tx, user_id, membership).await?);
    }
    tx.commit().await?;

    Ok(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user,
        workspaces,
    })
}

/// One workspace as [`bootstrap`] gives it, for a person who has just created or joined it
/// (`PUT /v1/workspaces/{id}`, `POST /v1/invitations/accept`): built by the same code, in the same
/// kind of snapshot, so the client subscribes `after = head` exactly as it does after a bootstrap.
/// `not_found` if the person is not an active member of it.
pub async fn joined_workspace(
    pool: &PgPool,
    user_id: UserId,
    workspace_id: WorkspaceId,
) -> Result<WorkspaceBootstrap, AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let membership: Option<Membership> = sqlx::query_as(membership_select!(" AND w.id = $2"))
        .bind(user_id.as_uuid())
        .bind(workspace_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
    let membership = membership.ok_or_else(|| AppError::not_found("workspace not found"))?;
    let workspace = workspace_bootstrap(&mut tx, user_id, membership).await?;
    tx.commit().await?;
    Ok(workspace)
}

/// Everything the person sees of one workspace they are an active member of, read in the caller's
/// snapshot.
async fn workspace_bootstrap(
    tx: &mut PgConnection,
    user_id: UserId,
    (workspace_id, name, head, role, settings): Membership,
) -> Result<WorkspaceBootstrap, AppError> {
    let role = db::role_from_db(&role)?;
    let workspace_id = WorkspaceId::from_uuid(workspace_id);

    let (conversations, user_state) =
        conversations::member_conversations(tx, user_id, workspace_id).await?;

    type MemberRow = (
        Uuid,
        String,
        String,
        bool,
        Option<String>,
        Option<String>,
        Option<time::OffsetDateTime>,
        bool,
        i32,
    );
    // Each person's status as it stands now: one that has ended is not served.
    let member_rows: Vec<MemberRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name, u.is_bot, us.text, us.emoji, us.expires_at, \
                (wm.deactivated_at IS NOT NULL) AS deactivated, u.avatar_version \
         FROM workspace_members wm JOIN users u ON u.id = wm.user_id \
         LEFT JOIN user_status us ON us.user_id = u.id AND us.workspace_id = wm.workspace_id \
              AND (us.expires_at IS NULL OR us.expires_at > now()) \
         WHERE wm.workspace_id = $1 ORDER BY wm.joined_at, u.id",
    )
    .bind(workspace_id.as_uuid())
    .fetch_all(&mut *tx)
    .await?;
    let members = member_rows
        .iter()
        .map(
            |(id, username, display_name, is_bot, text, emoji, expires_at, deactivated, avatar)| {
                let mut member = db::user_from_db(*id, username, display_name, *is_bot, *avatar)?;
                member.deactivated = *deactivated;
                if let Some(text) = text {
                    member.status = Some(notifications::status_from_row(
                        text,
                        emoji.as_deref(),
                        *expires_at,
                    )?);
                }
                Ok(member)
            },
        )
        .collect::<Result<Vec<_>, AppError>>()?;
    let (notification_settings, conversation_notification_prefs) =
        notifications::bootstrap_parts(tx, user_id, workspace_id).await?;

    Ok(WorkspaceBootstrap {
        workspace: Workspace {
            id: workspace_id,
            name,
            settings: settings.0,
        },
        role,
        conversations,
        user_state,
        groups: groups::groups_in_workspace(tx, workspace_id).await?,
        threads: threads::user_threads(tx, user_id, workspace_id).await?,
        custom_emoji: emoji::list(tx, workspace_id).await?,
        notification_settings,
        conversation_notification_prefs,
        saved: discovery::saved_ids(tx, user_id, workspace_id).await?,
        members,
        head: EventSeq::new(head)
            .ok_or_else(|| AppError::internal("workspace event head is negative"))?,
    })
}
