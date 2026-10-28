//! Registration, sign-in, and session authentication.

use lazy_domain::{UserId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{ErrorCode, Event, LoginRequest, RegisterRequest, SessionResponse, User};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::app::AppState;
use crate::conversations;
use crate::crypto;
use crate::db;
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};

/// The authenticated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principal {
    pub user_id: UserId,
    pub session_id: Uuid,
}

/// Creates an account, joins it to the default workspace and its public channels, and signs it
/// in. The membership change and its `MemberJoined` event commit atomically.
pub async fn register(
    state: &AppState,
    request: RegisterRequest,
) -> Result<SessionResponse, AppError> {
    if !state.options.open_registration {
        return Err(AppError::forbidden("registration is closed"));
    }
    crypto::check_password_policy(&request.password)?;
    let password_hash = state.passwords.hash(&request.password).await?;
    let user_id = UserId::generate();

    let mut tx = state.pool.begin().await?;
    let default_workspace: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM workspaces ORDER BY created_at, id LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    let Some(workspace_id) = default_workspace.map(WorkspaceId::from_uuid) else {
        return Err(AppError::forbidden("registration is closed"));
    };
    // Registrations take turns. Only other registrations wait on this lock, and it is taken
    // before any other, so it cannot take part in a deadlock; it makes "the first person to
    // register owns the workspace" hold even when two arrive together.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(workspace_id.to_string())
        .execute(&mut *tx)
        .await?;

    let inserted = sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id.as_uuid())
    .bind(request.username.as_str())
    .bind(request.display_name.as_str())
    .bind(&password_hash)
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(AppError::new(
                ErrorCode::UsernameTaken,
                "that username is taken",
            ));
        }
        Err(error) => return Err(error.into()),
    }

    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, \
         CASE WHEN EXISTS (SELECT 1 FROM workspace_members WHERE workspace_id = $1) \
              THEN 'member' ELSE 'owner' END)",
    )
    .bind(workspace_id.as_uuid())
    .bind(user_id.as_uuid())
    .execute(&mut *tx)
    .await?;
    // A new account joins the default channel only; other public channels are found by browsing.
    let default_channel =
        conversations::join_default_channel(&mut tx, workspace_id, user_id).await?;

    let user = User {
        id: user_id,
        username: request.username,
        display_name: request.display_name,
    };
    let mut announcements = vec![PendingEvent {
        kind: "member_joined",
        conversation_id: None,
        operation_id: None,
        audience: Audience::Workspace,
        event: Event::MemberJoined { user: user.clone() },
    }];
    if let Some(conversation) = default_channel {
        announcements.push(PendingEvent {
            kind: "member_added",
            conversation_id: Some(conversation.id),
            operation_id: None,
            audience: Audience::Members,
            event: Event::MemberAdded {
                conversation,
                user_id,
            },
        });
    }
    events::append_all(&mut tx, workspace_id, announcements).await?;
    let token = insert_session(&mut tx, user_id).await?;
    tx.commit().await?;

    Ok(SessionResponse { token, user })
}

/// Checks credentials and starts a session.
pub async fn login(state: &AppState, request: LoginRequest) -> Result<SessionResponse, AppError> {
    let row: Option<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, display_name, password_hash FROM users WHERE username = $1")
            .bind(request.username.as_str())
            .fetch_optional(&state.pool)
            .await?;

    let stored_hash = row.as_ref().map(|(_, _, hash)| hash.as_str());
    let verified = state
        .passwords
        .verify(&request.password, stored_hash)
        .await?;
    let Some((id, display_name, _)) = row.filter(|_| verified) else {
        return Err(AppError::new(
            ErrorCode::InvalidCredentials,
            "wrong username or password",
        ));
    };

    let user = db::user_from_db(id, request.username.as_str(), &display_name)?;
    let mut conn = state.pool.acquire().await?;
    let token = insert_session(&mut conn, user.id).await?;
    Ok(SessionResponse { token, user })
}

/// Resolves a bearer token to its session. Unknown, expired, and revoked tokens are the same
/// failure.
pub async fn authenticate(pool: &PgPool, token: &str) -> Result<Principal, AppError> {
    let hash = crypto::token_hash_if_wellformed(token).ok_or_else(AppError::unauthenticated)?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, user_id FROM sessions \
         WHERE token_hash = $1 AND NOT revoked AND expires_at > now()",
    )
    .bind(&hash[..])
    .fetch_optional(pool)
    .await?;
    let (session_id, user_id) = row.ok_or_else(AppError::unauthenticated)?;
    Ok(Principal {
        user_id: UserId::from_uuid(user_id),
        session_id,
    })
}

/// Whether a session is still usable. Long-lived connections poll this so that logging out ends
/// them.
pub async fn session_is_valid(pool: &PgPool, session_id: Uuid) -> Result<bool, AppError> {
    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = $1 AND NOT revoked AND expires_at > now())",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    Ok(valid)
}

/// Ends a session.
pub async fn logout(pool: &PgPool, principal: Principal) -> Result<(), AppError> {
    sqlx::query("UPDATE sessions SET revoked = true WHERE id = $1")
        .bind(principal.session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads the public profile of a user.
pub async fn load_user(pool: &PgPool, user_id: UserId) -> Result<User, AppError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT username, display_name FROM users WHERE id = $1")
            .bind(user_id.as_uuid())
            .fetch_optional(pool)
            .await?;
    let (username, display_name) = row.ok_or_else(AppError::unauthenticated)?;
    db::user_from_db(user_id.as_uuid(), &username, &display_name)
}

/// The role a user has in a workspace, if any.
pub async fn workspace_role(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    user_id: UserId,
) -> Result<Option<WorkspaceRole>, AppError> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id.as_uuid())
    .bind(user_id.as_uuid())
    .fetch_optional(pool)
    .await?;
    role.as_deref().map(db::role_from_db).transpose()
}

/// Sessions last 30 days from creation.
async fn insert_session(
    conn: &mut PgConnection,
    user_id: UserId,
) -> Result<lazy_protocol::SessionToken, AppError> {
    let (token, hash) = crypto::new_session_token()?;
    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, expires_at) \
         VALUES ($1, $2, $3, now() + interval '30 days')",
    )
    .bind(Uuid::now_v7())
    .bind(user_id.as_uuid())
    .bind(&hash[..])
    .execute(conn)
    .await?;
    Ok(token)
}
