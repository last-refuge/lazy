//! Connection pool, migrations, seeding, and conversions between database text and domain types.

use std::time::Duration;

use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, UserId,
    Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::User;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::error::AppError;

/// Opens a connection pool. The URL carries credentials, so errors are reported without it.
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, AppError> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .map_err(AppError::from)
}

/// Applies the migrations embedded in this binary.
pub async fn migrate(pool: &PgPool) -> Result<(), AppError> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| AppError::internal(format!("migration failed: {e}")))
}

/// What [`seed_default_workspace`] created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeededWorkspace {
    pub workspace_id: WorkspaceId,
    pub general_id: ConversationId,
}

/// Creates the workspace "Lazy" with a public `#general` channel, unless any workspace exists.
///
/// Safe to run from several servers at once: a transaction-scoped advisory lock serializes the
/// check and the insert. Returns `None` when nothing was created.
pub async fn seed_default_workspace(pool: &PgPool) -> Result<Option<SeededWorkspace>, AppError> {
    /// Arbitrary constant identifying the seed lock.
    const SEED_LOCK_KEY: i64 = 0x4c41_5a59_5345_4544;

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SEED_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM workspaces)")
        .fetch_one(&mut *tx)
        .await?;
    if exists {
        return Ok(None);
    }
    let workspace_id = WorkspaceId::generate();
    let general_id = ConversationId::generate();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, $2)")
        .bind(workspace_id.as_uuid())
        .bind("Lazy")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, name, topic, visibility, is_default) \
         VALUES ($1, $2, 'channel', $3, $4, 'public', true)",
    )
    .bind(general_id.as_uuid())
    .bind(workspace_id.as_uuid())
    .bind("general")
    .bind("Where everyone starts")
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(SeededWorkspace {
        workspace_id,
        general_id,
    }))
}

pub(crate) fn role_from_db(text: &str) -> Result<WorkspaceRole, AppError> {
    match text {
        "owner" => Ok(WorkspaceRole::Owner),
        "admin" => Ok(WorkspaceRole::Admin),
        "member" => Ok(WorkspaceRole::Member),
        other => Err(AppError::internal(format!(
            "unknown workspace role {other:?} in database"
        ))),
    }
}

pub(crate) fn visibility_from_db(text: &str) -> Result<ChannelVisibility, AppError> {
    match text {
        "public" => Ok(ChannelVisibility::Public),
        "private" => Ok(ChannelVisibility::Private),
        other => Err(AppError::internal(format!(
            "unknown visibility {other:?} in database"
        ))),
    }
}

pub(crate) fn visibility_to_db(visibility: ChannelVisibility) -> &'static str {
    match visibility {
        ChannelVisibility::Public => "public",
        ChannelVisibility::Private => "private",
    }
}

pub(crate) fn kind_from_db(text: &str) -> Result<ConversationKind, AppError> {
    match text {
        "channel" => Ok(ConversationKind::Channel),
        "direct" => Ok(ConversationKind::Direct),
        "group" => Ok(ConversationKind::Group),
        other => Err(AppError::internal(format!(
            "unknown conversation kind {other:?} in database"
        ))),
    }
}

pub(crate) fn kind_to_db(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Channel => "channel",
        ConversationKind::Direct => "direct",
        ConversationKind::Group => "group",
    }
}

pub(crate) fn user_from_db(id: Uuid, username: &str, display_name: &str) -> Result<User, AppError> {
    let username = Username::parse(username)
        .map_err(|e| AppError::internal(format!("stored username is invalid: {e}")))?;
    let display_name = DisplayName::parse(display_name)
        .map_err(|e| AppError::internal(format!("stored display name is invalid: {e}")))?;
    Ok(User {
        id: UserId::from_uuid(id),
        username,
        display_name,
    })
}

pub(crate) fn channel_name_from_db(text: &str) -> Result<ChannelName, AppError> {
    ChannelName::parse(text)
        .map_err(|e| AppError::internal(format!("stored channel name is invalid: {e}")))
}
