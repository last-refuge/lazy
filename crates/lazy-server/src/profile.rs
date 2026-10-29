//! A person's profile: display name and avatar (llm-docs/DECISIONS.md, D23).
//!
//! A profile belongs to the person, not to a workspace, so a change is announced to **every
//! workspace they are an active member of** (`UserProfileChanged`, once in each log) in the same
//! transaction that makes it.
//!
//! **Locking.** A change takes, in this order:
//!
//! 1. the person's advisory lock ([`lock_person`]), which also serializes the person's own
//!    creating and joining of workspaces, so the set of workspaces read below cannot change under
//!    the change, and a workspace they join at the same moment is announced to with the new profile
//!    or sees it in its member list, never the old one forever;
//! 2. the person's `users` row, by the update (a `FOR NO KEY UPDATE` lock: it does not block the
//!    foreign-key checks of their messages, and it makes setting a status, which reads the row
//!    `FOR SHARE`, take turns with it);
//! 3. each workspace's row, **in id order**, by `lock_workspace` and then the append.
//!
//! Every other transaction takes at most one workspace row at a time or the same locks in the same
//! order, so two people changing profiles at once, with workspaces in common, cannot deadlock.

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use lazy_domain::attachment::{MAX_IMAGE_PIXELS, is_previewable, sniff_media_type};
use lazy_domain::workspace::{AVATAR_SIZE_PX, MAX_AVATAR_UPLOAD_BYTES};
use lazy_domain::{DisplayName, UserId};
use lazy_protocol::{ErrorCode, Event, UpdateProfileRequest, User};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::app::AppState;
use crate::attachments::{self, images};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::{access, admin, db};

/// Takes the person's advisory lock, held until the transaction ends. The first lock of anything
/// that changes a person's profile or the set of workspaces they belong to by their own action.
pub(crate) async fn lock_person(conn: &mut PgConnection, user: UserId) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("person:{user}"))
        .execute(conn)
        .await?;
    Ok(())
}

fn avatar_not_found() -> AppError {
    AppError::not_found("avatar not found")
}

fn too_large() -> AppError {
    AppError::new(
        ErrorCode::PayloadTooLarge,
        format!("a picture can be at most {MAX_AVATAR_UPLOAD_BYTES} bytes"),
    )
}

/// What to change about a profile.
enum Change {
    Name(DisplayName),
    /// Set the avatar to this PNG.
    SetAvatar(Vec<u8>),
    RemoveAvatar,
}

/// Changes the caller's display name. Absent fields are unchanged; the name they already have
/// commits nothing and announces nothing.
pub async fn update_profile(
    pool: &PgPool,
    actor: UserId,
    request: UpdateProfileRequest,
) -> Result<User, AppError> {
    apply(pool, actor, request.display_name.map(Change::Name)).await
}

/// Removes the caller's avatar. Having none already commits nothing and announces nothing.
pub async fn remove_avatar(pool: &PgPool, actor: UserId) -> Result<User, AppError> {
    apply(pool, actor, Some(Change::RemoveAvatar)).await
}

/// Sets the caller's avatar from the image in the request body.
///
/// The format is found from the bytes (the `Content-Type` is only a hint, and ignored). A body
/// over [`MAX_AVATAR_UPLOAD_BYTES`] is refused (`payload_too_large`) by its declared length before
/// any of it is read, and otherwise as soon as it grows past the limit, so at most the limit is
/// ever held. The decoding runs on the blocking pool behind the same semaphore as thumbnails, with
/// the same pixel bound and orientation handling; what cannot be decoded is `unsupported_media`.
/// The result is a square PNG of exactly [`AVATAR_SIZE_PX`] pixels, cut from the centre; the
/// original is not kept. The same picture again changes nothing.
pub async fn set_avatar(
    state: &AppState,
    actor: UserId,
    headers: &HeaderMap,
    body: Body,
) -> Result<User, AppError> {
    // An app has no avatar: refused before reading anything.
    require_person(&state.pool, actor).await?;
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_AVATAR_UPLOAD_BYTES as u64) {
        return Err(too_large());
    }
    let bytes: std::sync::Arc<[u8]> = read_capped(body, MAX_AVATAR_UPLOAD_BYTES).await?.into();
    let media_type = sniff_media_type(&bytes[..bytes.len().min(32)])
        .filter(|media| is_previewable(media))
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::UnsupportedMedia,
                "a picture must be a PNG, JPEG, GIF or WebP image",
            )
        })?;
    let png = {
        let _slot = state
            .attachments
            .thumbnail_slots
            .acquire()
            .await
            .map_err(|_| AppError::internal("the image decoder is shut down"))?;
        tokio::task::spawn_blocking(move || {
            images::avatar_image(media_type, MAX_IMAGE_PIXELS, AVATAR_SIZE_PX, || {
                Ok(Box::new(std::io::Cursor::new(std::sync::Arc::clone(
                    &bytes,
                ))))
            })
        })
        .await
        .map_err(|e| AppError::internal(format!("image decoding failed: {e}")))?
        .map_err(|images::Refused(why)| AppError::new(ErrorCode::UnsupportedMedia, why))?
    };
    apply(&state.pool, actor, Some(Change::SetAvatar(png))).await
}

/// Reads a request body, stopping with `payload_too_large` the moment it exceeds `max` bytes.
async fn read_capped(body: Body, max: usize) -> Result<Vec<u8>, AppError> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| AppError::invalid("the upload was interrupted"))?;
        if chunk.len() > max - out.len() {
            return Err(too_large());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn require_person(pool: &PgPool, user: UserId) -> Result<(), AppError> {
    let is_bot: Option<bool> = sqlx::query_scalar("SELECT is_bot FROM users WHERE id = $1")
        .bind(user.as_uuid())
        .fetch_optional(pool)
        .await?;
    match is_bot {
        None => Err(AppError::unauthenticated()),
        Some(true) => Err(AppError::forbidden("an app has no profile to change")),
        Some(false) => Ok(()),
    }
}

/// Makes the change in one transaction and announces it to every workspace the person is an
/// active member of. Nothing is committed or announced if nothing changes.
async fn apply(pool: &PgPool, actor: UserId, change: Option<Change>) -> Result<User, AppError> {
    let mut tx = pool.begin().await?;
    lock_person(&mut tx, actor).await?;
    let row: Option<(String, String, bool, i32)> = sqlx::query_as(
        "SELECT username, display_name, is_bot, avatar_version FROM users WHERE id = $1",
    )
    .bind(actor.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let (username, display_name, is_bot, avatar_version) =
        row.ok_or_else(AppError::unauthenticated)?;
    if is_bot {
        return Err(AppError::forbidden("an app has no profile to change"));
    }
    let current = db::user_from_db(
        actor.as_uuid(),
        &username,
        &display_name,
        is_bot,
        avatar_version,
    )?;

    let mut name = current.display_name.clone();
    let mut version = avatar_version;
    match change {
        None => {}
        Some(Change::Name(new)) => name = new,
        Some(Change::RemoveAvatar) => {
            if avatar_version != 0 {
                // The row stays, without its picture, to remember the highest version used.
                sqlx::query(
                    "UPDATE user_avatars SET png = NULL, updated_at = now() WHERE user_id = $1",
                )
                .bind(actor.as_uuid())
                .execute(&mut *tx)
                .await?;
                version = 0;
            }
        }
        Some(Change::SetAvatar(png)) => {
            let stored: Option<(i32, bool)> = sqlx::query_as(
                "SELECT version, (png IS NOT NULL AND png = $2) FROM user_avatars \
                 WHERE user_id = $1",
            )
            .bind(actor.as_uuid())
            .bind(&png)
            .fetch_optional(&mut *tx)
            .await?;
            if !matches!(stored, Some((_, true))) {
                // A number is never used twice for one person, so a cached `?v=` URL cannot show
                // another picture than the one it was cached for.
                let next = stored
                    .map_or(Some(1), |(highest, _)| highest.checked_add(1))
                    .ok_or_else(|| AppError::internal("avatar version overflow"))?;
                sqlx::query(
                    "INSERT INTO user_avatars (user_id, version, png) VALUES ($1, $2, $3) \
                     ON CONFLICT (user_id) DO UPDATE \
                     SET version = EXCLUDED.version, png = EXCLUDED.png, updated_at = now()",
                )
                .bind(actor.as_uuid())
                .bind(next)
                .bind(&png)
                .execute(&mut *tx)
                .await?;
                version = next;
            }
        }
    }
    let user = User {
        display_name: name.clone(),
        avatar_version: u32::try_from(version)
            .map_err(|_| AppError::internal("avatar version is negative"))?,
        ..current.clone()
    };
    if name == current.display_name && version == avatar_version {
        return Ok(user);
    }
    sqlx::query("UPDATE users SET display_name = $2, avatar_version = $3 WHERE id = $1")
        .bind(actor.as_uuid())
        .bind(name.as_str())
        .bind(version)
        .execute(&mut *tx)
        .await?;

    // Every workspace the person has a membership of, in id order (the order every transaction
    // that locks several takes them in). Whether it is still an active one is decided under the
    // workspace's lock, so a deactivation or reactivation racing this is either before it
    // (decided accordingly) or after it (and the change was announced while they were a member).
    let workspaces: Vec<Uuid> = sqlx::query_scalar(
        "SELECT workspace_id FROM workspace_members WHERE user_id = $1 ORDER BY workspace_id",
    )
    .bind(actor.as_uuid())
    .fetch_all(&mut *tx)
    .await?;
    for workspace in workspaces {
        let workspace = lazy_domain::WorkspaceId::from_uuid(workspace);
        admin::lock_workspace(&mut tx, workspace).await?;
        if access::active_role(&mut tx, workspace, actor)
            .await?
            .is_none()
        {
            continue;
        }
        events::append_all(
            &mut tx,
            workspace,
            vec![PendingEvent {
                kind: "user_profile_changed",
                conversation_id: None,
                operation_id: None,
                audience: Audience::Workspace,
                event: Event::UserProfileChanged {
                    user_id: actor,
                    display_name: user.display_name.clone(),
                    avatar_version: user.avatar_version,
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(user)
}

/// Serves a person's current avatar as a PNG to someone who may see it: the person themself, or
/// someone who shares an **active** workspace with them (the membership predicate of
/// [`access`], on both sides, so a deactivated person neither sees nor is seen through that
/// workspace). Everything else (no such person, an app, no picture, a stranger) is the same
/// `not_found`.
///
/// The picture is always the person's *current* one, never anyone else's. When `?v=` is the
/// current version the response is `private, max-age=31536000, immutable` (the version is in the
/// URL, so it never changes); with a stale or absent `v` it is `no-cache`, so the client
/// revalidates by `ETag` and learns the new version from the profile event meanwhile.
pub async fn download_avatar(
    pool: &PgPool,
    viewer: UserId,
    person: UserId,
    requested_version: Option<u32>,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    // One statement, so the version and the picture are the same ones.
    let stored: Option<(i32, Vec<u8>)> = sqlx::query_as(concat!(
        "SELECT a.version, a.png FROM users u JOIN user_avatars a ON a.user_id = u.id \
         WHERE u.id = $2 AND NOT u.is_bot AND a.png IS NOT NULL AND u.avatar_version = a.version \
           AND ($1 = $2 OR EXISTS ( \
                 SELECT 1 FROM workspace_members mine JOIN workspace_members theirs \
                     ON theirs.workspace_id = mine.workspace_id \
                 WHERE mine.user_id = $1 AND theirs.user_id = $2 AND ",
        access::active_member!("mine"),
        " AND ",
        access::active_member!("theirs"),
        "))"
    ))
    .bind(viewer.as_uuid())
    .bind(person.as_uuid())
    .fetch_optional(pool)
    .await?;
    let (version, png) = stored.ok_or_else(avatar_not_found)?;
    let etag = format!("\"{version}\"");
    let immutable = requested_version.and_then(|v| i32::try_from(v).ok()) == Some(version);
    let mut response_headers = HeaderMap::new();
    let mut put = |name, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            response_headers.insert(name, value);
        }
    };
    put(ETAG, &etag);
    put(
        CACHE_CONTROL,
        if immutable {
            "private, max-age=31536000, immutable"
        } else {
            "private, no-cache"
        },
    );
    put(X_CONTENT_TYPE_OPTIONS, "nosniff");
    put(CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox");
    if headers
        .get(IF_NONE_MATCH)
        .is_some_and(|value| attachments::etag_matches(value, &etag))
    {
        return Ok((StatusCode::NOT_MODIFIED, response_headers).into_response());
    }
    put(CONTENT_TYPE, "image/png");
    put(CONTENT_LENGTH, &png.len().to_string());
    Ok((StatusCode::OK, response_headers, png).into_response())
}
