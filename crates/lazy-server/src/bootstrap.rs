//! The bootstrap snapshot that starts a client's synchronization.

use lazy_domain::{ChannelAccess, ConversationId, EventSeq, UserId, WorkspaceId};
use lazy_protocol::{
    BootstrapResponse, Conversation, PROTOCOL_VERSION, Workspace, WorkspaceBootstrap,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::AppError;

/// Returns the caller's workspaces with conversations, members, and the event-log position the
/// snapshot corresponds to.
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
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT username, display_name FROM users WHERE id = $1")
                .bind(user_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        let (username, display_name) = row.ok_or_else(AppError::unauthenticated)?;
        db::user_from_db(user_id.as_uuid(), &username, &display_name)?
    };

    let memberships: Vec<(Uuid, String, i64, String)> = sqlx::query_as(
        "SELECT w.id, w.name, w.event_head, wm.role \
         FROM workspace_members wm JOIN workspaces w ON w.id = wm.workspace_id \
         WHERE wm.user_id = $1 ORDER BY w.created_at, w.id",
    )
    .bind(user_id.as_uuid())
    .fetch_all(&mut *tx)
    .await?;

    let mut workspaces = Vec::with_capacity(memberships.len());
    for (workspace_id, name, head, role) in memberships {
        let role = db::role_from_db(&role)?;
        let workspace_id = WorkspaceId::from_uuid(workspace_id);

        let conversation_rows: Vec<(Uuid, String, Option<String>, String, bool, bool)> =
            sqlx::query_as(
                "SELECT c.id, c.name, c.topic, c.visibility, c.archived, (cm.user_id IS NOT NULL) \
                 FROM conversations c \
                 LEFT JOIN conversation_members cm ON cm.conversation_id = c.id AND cm.user_id = $1 \
                 WHERE c.workspace_id = $2 ORDER BY c.name",
            )
            .bind(user_id.as_uuid())
            .bind(workspace_id.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
        let mut conversations = Vec::new();
        for (id, name, topic, visibility, archived, is_member) in conversation_rows {
            let visibility = db::visibility_from_db(&visibility)?;
            let access = ChannelAccess {
                workspace_role: Some(role),
                is_channel_member: is_member,
                visibility,
                archived,
            };
            if access.can_read() {
                conversations.push(Conversation {
                    id: ConversationId::from_uuid(id),
                    workspace_id,
                    name: db::channel_name_from_db(&name)?,
                    topic,
                    visibility,
                    archived,
                });
            }
        }

        let member_rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT u.id, u.username, u.display_name \
             FROM workspace_members wm JOIN users u ON u.id = wm.user_id \
             WHERE wm.workspace_id = $1 ORDER BY wm.joined_at, u.id",
        )
        .bind(workspace_id.as_uuid())
        .fetch_all(&mut *tx)
        .await?;
        let members = member_rows
            .iter()
            .map(|(id, username, display_name)| db::user_from_db(*id, username, display_name))
            .collect::<Result<Vec<_>, _>>()?;

        workspaces.push(WorkspaceBootstrap {
            workspace: Workspace {
                id: workspace_id,
                name,
            },
            role,
            conversations,
            members,
            head: EventSeq::new(head)
                .ok_or_else(|| AppError::internal("workspace event head is negative"))?,
        });
    }
    tx.commit().await?;

    Ok(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user,
        workspaces,
    })
}
