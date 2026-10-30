//! What a new client may ask before anyone has signed in (D29): how the server takes new accounts,
//! and what an invitation code is for.
//!
//! Neither route creates or uses anything. They exist so that the first screens can say "you're
//! invited to Zebrafish Collective" before a person has chosen a password, and so that a person who
//! typed the wrong server can see that before they give it anything.
//!
//! **What is revealed, and to whom.** `GET /v1/server` holds no name and no count: a stranger who
//! finds the address learns the protocol, whether registration is open, and whether the server is
//! waiting for a setup code (only when a setup code is configured, so an unclaimed server with none
//! is not advertised as unclaimed). `POST /v1/invitations/preview` gives the workspace's name, who
//! made the invitation, and how many people are in the workspace only for a code that is valid at
//! this moment; every other code, of any shape, is one `invitation_invalid` with one message, the
//! same as at registration, so the route is no better an oracle than registering is. A code is
//! 100 bits, which is what keeps it from being guessed.

use lazy_domain::DisplayName;
use lazy_domain::workspace::{
    RegistrationPolicy, WorkspaceSettings, canonical_invite_code, invite_code_is_wellformed,
};
use lazy_protocol::{
    InvitationKind, InvitationPreview, PROTOCOL_VERSION, PreviewInvitationRequest,
    ServerInfoResponse, ServerRegistration,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::accounts::{invitation_invalid, workspace_has_no_members};
use crate::app::AppState;
use crate::crypto;
use crate::error::AppError;

/// The workspace a registration without an invitation joins: the oldest one no person created.
struct DefaultWorkspace {
    id: Uuid,
    name: String,
    settings: WorkspaceSettings,
}

async fn default_workspace(pool: &PgPool) -> Result<Option<DefaultWorkspace>, AppError> {
    let row: Option<(Uuid, String, sqlx::types::Json<WorkspaceSettings>)> = sqlx::query_as(
        "SELECT id, name, settings FROM workspaces WHERE created_by IS NULL \
         ORDER BY created_at, id LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, name, settings)| DefaultWorkspace {
        id,
        name,
        settings: settings.0,
    }))
}

/// Whether the default workspace is waiting for its first account, which needs the setup code.
async fn waiting_for_setup_code(
    state: &AppState,
    default: Option<&DefaultWorkspace>,
) -> Result<bool, AppError> {
    let (Some(_), Some(default)) = (&state.integrations.setup_code, default) else {
        return Ok(false);
    };
    let mut conn = state.pool.acquire().await?;
    workspace_has_no_members(&mut conn, lazy_domain::WorkspaceId::from_uuid(default.id)).await
}

/// `GET /v1/server`.
pub async fn server_info(state: &AppState) -> Result<ServerInfoResponse, AppError> {
    let default = default_workspace(&state.pool).await?;
    let setup_code_required = waiting_for_setup_code(state, default.as_ref()).await?;
    let registration = match &default {
        _ if !state.options.open_registration => ServerRegistration::Closed,
        // No workspace without a creator to register into: only an invitation gets a person in.
        None => ServerRegistration::InviteOnly,
        // The first account needs the setup code, which goes where an invitation code does.
        Some(_) if setup_code_required => ServerRegistration::InviteOnly,
        Some(default) => match default.settings.registration {
            RegistrationPolicy::Open => ServerRegistration::Open,
            RegistrationPolicy::InviteOnly => {
                // An invite-only workspace with no members and no setup code lets the first person
                // in, as registration does, so for that person it is open.
                let mut conn = state.pool.acquire().await?;
                let empty = workspace_has_no_members(
                    &mut conn,
                    lazy_domain::WorkspaceId::from_uuid(default.id),
                )
                .await?;
                if empty {
                    ServerRegistration::Open
                } else {
                    ServerRegistration::InviteOnly
                }
            }
        },
    };
    Ok(ServerInfoResponse {
        protocol_version: PROTOCOL_VERSION,
        registration,
        setup_code_required,
    })
}

/// `POST /v1/invitations/preview`.
pub async fn preview_invitation(
    state: &AppState,
    request: PreviewInvitationRequest,
) -> Result<InvitationPreview, AppError> {
    if !state.options.open_registration {
        return Err(AppError::forbidden("registration is closed"));
    }
    let given = request.code.expose();

    // The setup code, while the server waits for one. Compared as registration compares it.
    let default = default_workspace(&state.pool).await?;
    if let Some(setup_code) = &state.integrations.setup_code
        && waiting_for_setup_code(state, default.as_ref()).await?
        && setup_code.matches(given)
        && let Some(default) = default
    {
        return Ok(InvitationPreview {
            kind: InvitationKind::Setup,
            workspace_name: default.name,
            inviter: None,
            member_count: 0,
        });
    }

    // Otherwise an invitation, in the form every place that reads a code puts it in.
    let code = canonical_invite_code(given);
    if !invite_code_is_wellformed(&code) {
        return Err(invitation_invalid());
    }
    let hash = crypto::invite_code_hash(&code);
    // The same conditions as the guarded `UPDATE` that registration uses to count a use, read
    // without changing anything.
    let row: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT w.name, u.display_name, \
                (SELECT count(*) FROM workspace_members m \
                  WHERE m.workspace_id = w.id AND m.deactivated_at IS NULL) \
         FROM workspace_invitations i \
         JOIN workspaces w ON w.id = i.workspace_id \
         JOIN users u ON u.id = i.created_by \
         WHERE i.code_hash = $1 AND i.revoked_at IS NULL AND i.expires_at > now() \
           AND (i.max_uses IS NULL OR i.uses < i.max_uses)",
    )
    .bind(&hash[..])
    .fetch_optional(&state.pool)
    .await?;
    let (workspace_name, inviter, members) = row.ok_or_else(invitation_invalid)?;
    Ok(InvitationPreview {
        kind: InvitationKind::Invitation,
        workspace_name,
        inviter: DisplayName::parse(&inviter).ok(),
        member_count: u32::try_from(members).unwrap_or(u32::MAX),
    })
}
