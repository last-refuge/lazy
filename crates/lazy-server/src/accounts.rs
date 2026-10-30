//! Registration, sign-in, and session authentication.

use lazy_domain::workspace::{
    RegistrationPolicy, WorkspaceSettings, canonical_invite_code, invite_code_is_wellformed,
};
use lazy_domain::{UserId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{
    Conversation, ErrorCode, Event, LoginRequest, RegisterRequest, SessionResponse, User,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::app::AppState;
use crate::conversations;
use crate::crypto;
use crate::db;
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::groups;
use crate::webhooks;

/// The one answer for an invitation that cannot be used, whether the code is of the wrong shape,
/// unknown, revoked, expired or used up: nothing says which, so codes cannot be probed.
pub(crate) fn invitation_invalid() -> AppError {
    AppError::new(ErrorCode::InvitationInvalid, "that invitation is not valid")
}

/// What a registration is told while the server waits for its first account and the setup code was
/// missing or wrong (the same answer, so the message does not say which).
pub const SETUP_CODE_REQUIRED: &str =
    "this server needs its setup code to create the first account";

/// Takes the registration lock of a workspace, held until the transaction ends.
async fn lock_registration(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(workspace_id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn workspace_has_no_members(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM workspace_members WHERE workspace_id = $1)",
    )
    .bind(workspace_id.as_uuid())
    .fetch_one(conn)
    .await?)
}

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
    // A person is mentioned by their username, so words that mean something else in a mention
    // (`channel`, and `here` and `everyone`, reserved for later) cannot be one.
    if groups::username_is_reserved(request.username.as_str()) {
        return Err(AppError::invalid("that username is reserved"));
    }
    // The names generated for webhook bots are theirs: a person cannot take one first.
    if webhooks::is_bot_username(request.username.as_str()) {
        return Err(AppError::invalid(
            "that username is reserved for integrations",
        ));
    }
    let password_hash = state.passwords.hash(&request.password).await?;
    let user_id = UserId::generate();

    let mut tx = state.pool.begin().await?;
    let candidate = request.invite_code.as_ref().map(|code| code.expose());

    // The workspace a registration without an invitation joins: the oldest one no person created
    // (the seeded one), so a workspace made with `PUT /v1/workspaces/{id}` is never where an open
    // registration lands.
    let default: Option<(Uuid, sqlx::types::Json<WorkspaceSettings>)> = sqlx::query_as(
        "SELECT id, settings FROM workspaces WHERE created_by IS NULL \
         ORDER BY created_at, id LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;

    // The setup state: a server with a setup code, whose default workspace has no members. The
    // first account there needs the code (given as the invitation code), and nothing else gets in.
    // Members are never removed, so once there is one this stays false for good, and the check
    // costs nothing but a lookup; while there is none, the registration lock is taken *before*
    // looking again, so of two first registrations with the right code the second finds a member
    // and goes through the ordinary path (where the code is only an invitation code, and wrong).
    let mut in_setup = false;
    if let (Some(setup_code), Some((default_id, _))) = (&state.integrations.setup_code, &default) {
        let default_id = WorkspaceId::from_uuid(*default_id);
        if workspace_has_no_members(&mut tx, default_id).await? {
            lock_registration(&mut tx, default_id).await?;
            if workspace_has_no_members(&mut tx, default_id).await? {
                // One answer for a missing code and a wrong one.
                if !candidate.is_some_and(|code| setup_code.matches(code)) {
                    return Err(AppError::forbidden(SETUP_CODE_REQUIRED));
                }
                in_setup = true;
            }
        }
    }

    // An invitation code, if one was sent: the hash is what is looked up. A code of the wrong shape
    // is as invalid as a wrong one, and is refused after the same work (the password is already
    // hashed), with the same answer. (Not in the setup state: there the code is the setup code.)
    let invite_hash = match candidate {
        _ if in_setup => None,
        None => None,
        Some(code) => {
            // Written any way a person might write it (case, groups, dashes), it is the same code.
            let code = canonical_invite_code(code);
            if !invite_code_is_wellformed(&code) {
                return Err(invitation_invalid());
            }
            Some(crypto::invite_code_hash(&code))
        }
    };

    let workspace_id = match (&invite_hash, &default) {
        (None, Some((id, _))) if in_setup => WorkspaceId::from_uuid(*id),
        // The invitation says which workspace.
        (Some(hash), _) => {
            let found: Option<Uuid> = sqlx::query_scalar(
                "SELECT workspace_id FROM workspace_invitations WHERE code_hash = $1",
            )
            .bind(&hash[..])
            .fetch_optional(&mut *tx)
            .await?;
            WorkspaceId::from_uuid(found.ok_or_else(invitation_invalid)?)
        }
        // Otherwise the default workspace, if its policy lets anyone register.
        (None, Some((id, settings))) => {
            let id = WorkspaceId::from_uuid(*id);
            // Under the registration lock, so that "has no members" cannot change while it is
            // being decided.
            lock_registration(&mut tx, id).await?;
            // An invite-only workspace with no members (a server seeded that way) lets the first
            // person in, who becomes its owner: nobody could have invited them. A server that is
            // reachable by strangers should set a setup code, which this does not replace.
            if settings.0.registration == RegistrationPolicy::InviteOnly
                && !workspace_has_no_members(&mut tx, id).await?
            {
                return Err(AppError::forbidden("registration is by invitation"));
            }
            id
        }
        (None, None) => return Err(AppError::forbidden("registration is closed")),
    };
    // Registrations take turns. Only other registrations wait on this lock, and it is taken
    // before any other, so it cannot take part in a deadlock; it makes "the first person to
    // register owns the workspace" hold even when two arrive together.
    lock_registration(&mut tx, workspace_id).await?;
    if let Some(hash) = &invite_hash {
        // The use is counted by one guarded statement under the invitation's row lock: with
        // `max_uses` 1, of two registrations at once the second waits, finds it used up, and fails.
        // Everything below is in this transaction, so a registration that fails afterwards (the
        // username is taken) gives the use back.
        let used: Option<Uuid> = sqlx::query_scalar(
            "UPDATE workspace_invitations SET uses = uses + 1 \
             WHERE code_hash = $1 AND revoked_at IS NULL AND expires_at > now() \
               AND (max_uses IS NULL OR uses < max_uses) RETURNING id",
        )
        .bind(&hash[..])
        .fetch_optional(&mut *tx)
        .await?;
        used.ok_or_else(invitation_invalid)?;
    }
    // Claim the username in the mention namespace it shares with user groups. The lock is taken
    // before looking, and held to commit, so a group being created with this handle at the same
    // moment either commits first (and we see it here) or waits for us (and sees the new person).
    groups::lock_handle(&mut tx, request.username.as_str()).await?;
    if groups::handle_names_a_group(&mut tx, request.username.as_str()).await? {
        return Err(AppError::new(
            ErrorCode::UsernameTaken,
            "that username is taken",
        ));
    }

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
        is_bot: false,
        status: None,
        deactivated: false,
        avatar_version: 0,
    };
    announce_member(&mut tx, workspace_id, user.clone(), default_channel).await?;
    let token = insert_session(&mut tx, user_id).await?;
    tx.commit().await?;

    Ok(SessionResponse { token, user })
}

/// Tells a workspace that someone joined it (`MemberJoined`, to everyone) and, if it has a default
/// channel, that they are in it (`MemberAdded`, to the channel). Used by registration and by
/// accepting an invitation, so both announce a new member identically. Call it last in the
/// transaction that added them.
pub(crate) async fn announce_member(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
    user: User,
    default_channel: Option<Conversation>,
) -> Result<(), AppError> {
    let user_id = user.id;
    let mut announcements = vec![PendingEvent {
        kind: "member_joined",
        conversation_id: None,
        operation_id: None,
        audience: Audience::Workspace,
        event: Event::MemberJoined { user },
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
    events::append_all(conn, workspace_id, announcements).await?;
    Ok(())
}

/// Checks credentials and starts a session.
pub async fn login(state: &AppState, request: LoginRequest) -> Result<SessionResponse, AppError> {
    let row: Option<(Uuid, String, String, bool, i32)> = sqlx::query_as(
        "SELECT id, display_name, password_hash, is_bot, avatar_version FROM users \
         WHERE username = $1",
    )
    .bind(request.username.as_str())
    .fetch_optional(&state.pool)
    .await?;

    // A bot has no password. It is treated exactly as an account that does not exist: the same
    // work is done against a stand-in hash, and the same error comes back, so signing in as one
    // cannot be told apart from a wrong password.
    let row = row.filter(|(_, _, _, is_bot, _)| !is_bot);
    let stored_hash = row.as_ref().map(|(_, _, hash, _, _)| hash.as_str());
    let verified = state
        .passwords
        .verify(&request.password, stored_hash)
        .await?;
    let Some((id, display_name, _, _, avatar_version)) = row.filter(|_| verified) else {
        return Err(AppError::new(
            ErrorCode::InvalidCredentials,
            "wrong username or password",
        ));
    };

    let user = db::user_from_db(
        id,
        request.username.as_str(),
        &display_name,
        false,
        avatar_version,
    )?;
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
    let row: Option<(String, String, bool, i32)> = sqlx::query_as(
        "SELECT username, display_name, is_bot, avatar_version FROM users WHERE id = $1",
    )
    .bind(user_id.as_uuid())
    .fetch_optional(pool)
    .await?;
    let (username, display_name, is_bot, avatar_version) =
        row.ok_or_else(AppError::unauthenticated)?;
    db::user_from_db(
        user_id.as_uuid(),
        &username,
        &display_name,
        is_bot,
        avatar_version,
    )
}

/// The role a user has in a workspace, if they have an active membership of it (a deactivated one
/// is no membership: see [`crate::access`]).
pub async fn workspace_role(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    user_id: UserId,
) -> Result<Option<WorkspaceRole>, AppError> {
    let mut conn = pool.acquire().await?;
    crate::access::active_role(&mut conn, workspace_id, user_id).await
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
