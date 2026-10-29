//! Connection pool, migrations, seeding, and conversions between database text and domain types.

use std::time::Duration;

use lazy_domain::workspace::WorkspaceSettings;
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, UserId,
    Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::User;
use sqlx::ConnectOptions;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppError;

/// Opens a connection pool. The URL carries credentials, so errors are reported without it.
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, AppError> {
    let options: PgConnectOptions = database_url.parse().map_err(AppError::from)?;
    connect_with(quiet(options), max_connections).await
}

/// Opens a pool with the given options (already [`quiet`] if they should be).
pub async fn connect_with(
    options: PgConnectOptions,
    max_connections: u32,
) -> Result<PgPool, AppError> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .map_err(AppError::from)
}

/// The options with SQLx's statement logging off, both the ordinary and the slow-statement kind:
/// it is the one logger that writes the text of statements, and a statement that was built with
/// data in it would carry the data into the log. (Values bound to `$n` are never logged by SQLx;
/// this makes that not depend on it.)
pub fn quiet(options: PgConnectOptions) -> PgConnectOptions {
    options.disable_statement_logging()
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
    seed_default_workspace_with(pool, WorkspaceSettings::default()).await
}

/// [`seed_default_workspace`] with the given settings for the workspace it creates (its
/// registration policy, for a server that should not be open to everyone from the first minute).
/// When a workspace already exists nothing is created and the settings are ignored.
pub async fn seed_default_workspace_with(
    pool: &PgPool,
    settings: WorkspaceSettings,
) -> Result<Option<SeededWorkspace>, AppError> {
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
    let general_id = insert_workspace(&mut tx, workspace_id, "Lazy", None)
        .await?
        .ok_or_else(|| AppError::internal("a new workspace id was already taken"))?;
    if settings != WorkspaceSettings::default() {
        sqlx::query("UPDATE workspaces SET settings = $2 WHERE id = $1")
            .bind(workspace_id.as_uuid())
            .bind(sqlx::types::Json(settings))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Some(SeededWorkspace {
        workspace_id,
        general_id,
    }))
}

/// Whether the workspace registration targets (the oldest with no creator: the seeded one) has no
/// members yet; `None` if there is no such workspace.
pub async fn default_workspace_is_empty(pool: &PgPool) -> Result<Option<bool>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM workspace_members m WHERE m.workspace_id = w.id) \
         FROM workspaces w WHERE w.created_by IS NULL ORDER BY w.created_at, w.id LIMIT 1",
    )
    .fetch_optional(pool)
    .await?)
}

/// Inserts a workspace with default settings and its default channel, `#general`, and returns the
/// channel's id; `None` (nothing inserted) if the id is taken. `created_by` is the person who made
/// it, or `None` for the one the server seeds. Nobody is a member yet: the caller adds them in the
/// same transaction.
pub(crate) async fn insert_workspace(
    conn: &mut PgConnection,
    id: WorkspaceId,
    name: &str,
    created_by: Option<UserId>,
) -> Result<Option<ConversationId>, AppError> {
    let inserted = sqlx::query(
        "INSERT INTO workspaces (id, name, created_by) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(name)
    .bind(created_by.map(|user| user.as_uuid()))
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(None);
    }
    let general_id = ConversationId::generate();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, name, topic, visibility, is_default) \
         VALUES ($1, $2, 'channel', $3, $4, 'public', true)",
    )
    .bind(general_id.as_uuid())
    .bind(id.as_uuid())
    .bind("general")
    .bind("Where everyone starts")
    .execute(&mut *conn)
    .await?;
    Ok(Some(general_id))
}

pub(crate) fn role_to_db(role: WorkspaceRole) -> &'static str {
    match role {
        WorkspaceRole::Owner => "owner",
        WorkspaceRole::Admin => "admin",
        WorkspaceRole::Member => "member",
    }
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

pub(crate) fn user_from_db(
    id: Uuid,
    username: &str,
    display_name: &str,
    is_bot: bool,
    avatar_version: i32,
) -> Result<User, AppError> {
    let username =
        Username::parse(username).map_err(|_| AppError::internal("stored username is invalid"))?;
    let display_name = DisplayName::parse(display_name)
        .map_err(|_| AppError::internal("stored display name is invalid"))?;
    Ok(User {
        id: UserId::from_uuid(id),
        username,
        display_name,
        is_bot,
        status: None,
        deactivated: false,
        avatar_version: u32::try_from(avatar_version)
            .map_err(|_| AppError::internal("stored avatar version is negative"))?,
    })
}

pub(crate) fn channel_name_from_db(text: &str) -> Result<ChannelName, AppError> {
    ChannelName::parse(text).map_err(|_| AppError::internal("stored channel name is invalid"))
}
