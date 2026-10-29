//! Custom emoji (llm-docs/DECISIONS.md, D18): a workspace's own images, named, usable in messages
//! and as reactions, managed by admins and owners.
//!
//! The row says what the emoji is and whether it is live; the image is a small PNG in the
//! attachment store under the emoji's id. Messages and reactions name an emoji by its id, so a
//! rename changes how it reads everywhere, and a deleted emoji keeps its row (and shows as muted
//! `:name:`) while its image is removed by the cleanup job.
//!
//! # Locks
//!
//! Creating, renaming and deleting take `emoji:{workspace}` (advisory, so the name and the limit of
//! 500 are checked and written as one step) and then the emoji's own row. A message or reaction
//! that uses an emoji locks its row `FOR SHARE` in the same transaction that commits it, before
//! the workspace row, so a deletion (which updates the row) and a use take turns: the use commits
//! before the deletion, or finds the emoji gone and is refused. Nothing is ever committed that
//! uses an emoji deleted before it.

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use lazy_domain::attachment::{is_previewable, sniff_media_type};
use lazy_domain::emoji::{
    EMOJI_IMAGE_EDGE, MAX_CUSTOM_EMOJI, MAX_EMOJI_BYTES, MAX_EMOJI_SOURCE_PIXELS,
};
use lazy_domain::{
    EmojiId, EmojiName, MessageDocument, ReactionEmoji, Timestamp, UserId, WorkspaceId,
};
use lazy_protocol::{CustomEmoji, ErrorCode, Event};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::accounts;
use crate::app::AppState;
use crate::attachments::store::{AttachmentStore, ObjectKey, StoreError};
use crate::attachments::{self, images};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};

#[derive(sqlx::FromRow)]
struct EmojiRow {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    created_by: Uuid,
    created_at: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
    sha256: Vec<u8>,
}

macro_rules! emoji_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, workspace_id, name, created_by, created_at, deleted_at, sha256 \
             FROM custom_emoji ",
            $tail
        )
    };
}

impl EmojiRow {
    fn to_dto(&self) -> CustomEmoji {
        CustomEmoji {
            id: EmojiId::from_uuid(self.id),
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            name: self.name.clone(),
            created_by: UserId::from_uuid(self.created_by),
            created_at: Timestamp::from(self.created_at),
            deleted: self.deleted_at.is_some(),
        }
    }
}

fn emoji_not_found() -> AppError {
    AppError::not_found("emoji not found")
}

fn name_taken() -> AppError {
    AppError::new(ErrorCode::NameTaken, "that emoji name is taken")
}

fn manage_error() -> AppError {
    AppError::forbidden("only admins and owners manage custom emoji")
}

fn conflict() -> AppError {
    AppError::conflict("that emoji id is already used for something else")
}

async fn find(conn: &mut PgConnection, id: Uuid) -> Result<Option<EmojiRow>, AppError> {
    Ok(sqlx::query_as(emoji_select!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

/// The workspace's emoji, live and deleted, by name and then id.
pub(crate) async fn list(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<CustomEmoji>, AppError> {
    let rows: Vec<EmojiRow> =
        sqlx::query_as(emoji_select!("WHERE workspace_id = $1 ORDER BY name, id"))
            .bind(workspace.as_uuid())
            .fetch_all(conn)
            .await?;
    Ok(rows.iter().map(EmojiRow::to_dto).collect())
}

async fn advisory(conn: &mut PgConnection, name: String) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(name)
        .execute(conn)
        .await?;
    Ok(())
}

async fn announce(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    workspace: WorkspaceId,
    emoji: CustomEmoji,
) -> Result<(), AppError> {
    events::append_all(
        tx,
        workspace,
        vec![PendingEvent {
            kind: "emoji_changed",
            conversation_id: None,
            operation_id: None,
            audience: Audience::Workspace,
            event: Event::EmojiChanged { emoji },
        }],
    )
    .await?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a live emoji with this name exists in the workspace, other than `except`.
async fn live_name_taken(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    name: &str,
    except: Option<Uuid>,
) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM custom_emoji WHERE workspace_id = $1 AND name = $2 \
                        AND deleted_at IS NULL AND id IS DISTINCT FROM $3)",
    )
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(except)
    .fetch_one(conn)
    .await?)
}

async fn live_count(conn: &mut PgConnection, workspace: WorkspaceId) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM custom_emoji WHERE workspace_id = $1 AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_one(conn)
    .await?)
}

/// What an upload does when the id already exists: the same emoji again (same workspace, same
/// name, same bytes) returns it; anything else is a `conflict` that says no more.
fn repeat_of(
    existing: &EmojiRow,
    workspace: WorkspaceId,
    name: &EmojiName,
    sha256: Option<&[u8; 32]>,
) -> Result<CustomEmoji, AppError> {
    let same = existing.workspace_id == workspace.as_uuid()
        && existing.deleted_at.is_none()
        && existing.name == name.as_str()
        && sha256.is_none_or(|hash| existing.sha256 == hash[..]);
    if same {
        Ok(existing.to_dto())
    } else {
        Err(conflict())
    }
}

/// Adds a custom emoji from an uploaded image.
///
/// In order: the caller must be an admin or owner of the workspace (`not_found` for someone
/// outside it, `forbidden` for a member); the name comes from `x-emoji-name` (`invalid_request`
/// unless it is a valid name that is not a standard shortcode); `Content-Length` is required and
/// within [`MAX_EMOJI_BYTES`] (`payload_too_large`, before any byte is read); a live name already
/// taken is `name_taken` and 500 live emoji is `limit_reached`; then the body is streamed to a
/// staging file as an attachment's is, must be a PNG, JPEG, GIF or WebP that decodes within
/// [`MAX_EMOJI_SOURCE_PIXELS`] (`unsupported_media`), and is turned into a PNG whose long side is at
/// most [`EMOJI_IMAGE_EDGE`] (upright, aspect kept, never enlarged, the first frame of a GIF). The
/// original bytes are not kept. Finally, under locks, the PNG is stored, the row written last, and
/// `emoji_changed` appended, in one transaction. The same id with the same name and bytes is the
/// same emoji (200), whether the first upload has finished or is racing this one; with anything else
/// it is a `conflict`.
pub async fn upload(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    id: EmojiId,
    headers: &HeaderMap,
    body: Body,
) -> Result<CustomEmoji, AppError> {
    let pool = &state.pool;
    match accounts::workspace_role(pool, workspace, actor).await? {
        None => return Err(AppError::not_found("workspace not found")),
        Some(role) if !role.can_manage_emoji() => return Err(manage_error()),
        Some(_) => {}
    }
    let name = headers
        .get(lazy_protocol::api::paths::EMOJI_NAME_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::invalid("x-emoji-name is required"))
        .and_then(|text| EmojiName::parse(text).map_err(|e| AppError::invalid(e.to_string())))?;
    attachments::declared_length(headers, MAX_EMOJI_BYTES)?;

    let mut conn = pool.acquire().await?;
    match find(&mut conn, id.as_uuid()).await? {
        Some(existing) => {
            repeat_of(&existing, workspace, &name, None)?;
        }
        None => {
            if live_name_taken(&mut conn, workspace, name.as_str(), None).await? {
                return Err(name_taken());
            }
            if live_count(&mut conn, workspace).await? >= MAX_CUSTOM_EMOJI as i64 {
                return Err(too_many());
            }
        }
    }
    drop(conn);

    let attachments = &state.attachments;
    let staged = attachments
        .store
        .stage(
            id.as_uuid(),
            attachments::body_stream(body),
            MAX_EMOJI_BYTES,
        )
        .await
        .map_err(|error| match error {
            StoreError::TooLarge => AppError::new(
                ErrorCode::PayloadTooLarge,
                format!("an emoji image can be at most {MAX_EMOJI_BYTES} bytes"),
            ),
            StoreError::Io(e) if e.kind() == std::io::ErrorKind::ConnectionAborted => {
                AppError::invalid("the upload was interrupted")
            }
            StoreError::Io(e) => AppError::internal(format!("cannot store the upload: {e}")),
        })?;
    let result = finish_upload(state, actor, workspace, id, &name, &staged).await;
    // The original bytes are never kept: the staged file is dropped whatever happened.
    attachments.store.discard(&staged.token).await;
    result
}

fn too_many() -> AppError {
    AppError::new(
        ErrorCode::LimitReached,
        format!("a workspace can have at most {MAX_CUSTOM_EMOJI} custom emoji"),
    )
}

async fn finish_upload(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    id: EmojiId,
    name: &EmojiName,
    staged: &crate::attachments::store::Staged,
) -> Result<CustomEmoji, AppError> {
    let attachments = &state.attachments;
    let pool = &state.pool;
    if let Some(existing) = find(&mut *pool.acquire().await?, id.as_uuid()).await? {
        return repeat_of(&existing, workspace, name, Some(&staged.sha256));
    }

    let media_type = sniff_media_type(&staged.prefix)
        .filter(|media| is_previewable(media))
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::UnsupportedMedia,
                "a custom emoji must be a PNG, JPEG, GIF or WebP image",
            )
        })?;
    let image = {
        let _slot = attachments
            .thumbnail_slots
            .acquire()
            .await
            .map_err(|_| AppError::internal("the image decoder is shut down"))?;
        let store = std::sync::Arc::clone(&attachments.store);
        let token = staged.token.clone();
        tokio::task::spawn_blocking(move || {
            images::emoji_image(
                media_type,
                MAX_EMOJI_SOURCE_PIXELS,
                EMOJI_IMAGE_EDGE,
                || store.staged_source(&token),
            )
        })
        .await
        .map_err(|e| AppError::internal(format!("image decoding failed: {e}")))?
        .map_err(|images::Refused(why)| AppError::new(ErrorCode::UnsupportedMedia, why))?
    };

    let mut tx = pool.begin().await?;
    advisory(&mut tx, format!("emoji:{workspace}")).await?;
    advisory(&mut tx, format!("emoji-id:{id}")).await?;
    if let Some(existing) = find(&mut tx, id.as_uuid()).await? {
        return repeat_of(&existing, workspace, name, Some(&staged.sha256));
    }
    if live_name_taken(&mut tx, workspace, name.as_str(), None).await? {
        return Err(name_taken());
    }
    if live_count(&mut tx, workspace).await? >= MAX_CUSTOM_EMOJI as i64 {
        return Err(too_many());
    }

    let key = ObjectKey::emoji(id.as_uuid());
    attachments
        .store
        .put_small(key, image.png.clone())
        .await
        .map_err(|e| AppError::internal(format!("cannot store the image: {e}")))?;
    let written = async {
        if attachments.options.fail_between_file_and_row {
            return Err(AppError::internal(
                "simulated failure between the image and its row",
            ));
        }
        let (created_at,): (OffsetDateTime,) = sqlx::query_as(
            "INSERT INTO custom_emoji (id, workspace_id, name, created_by, size, sha256, width, height) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING created_at",
        )
        .bind(id.as_uuid())
        .bind(workspace.as_uuid())
        .bind(name.as_str())
        .bind(actor.as_uuid())
        .bind(image.png.len() as i64)
        .bind(&staged.sha256[..])
        .bind(image.width as i32)
        .bind(image.height as i32)
        .fetch_one(&mut *tx)
        .await?;
        let emoji = CustomEmoji {
            id,
            workspace_id: workspace,
            name: name.as_str().to_owned(),
            created_by: actor,
            created_at: Timestamp::from(created_at),
            deleted: false,
        };
        announce(&mut tx, workspace, emoji.clone()).await?;
        Ok::<CustomEmoji, AppError>(emoji)
    }
    .await;
    let committed = match written {
        Ok(emoji) => tx.commit().await.map(|()| emoji).map_err(AppError::from),
        Err(error) => Err(error),
    };
    if committed.is_err() && !attachments.options.fail_between_file_and_row {
        let _ = attachments.store.delete(key).await;
    }
    committed
}

/// Renames a custom emoji. Admins and owners of its workspace only. A name that is already a live
/// emoji's is `name_taken`; renaming to its own name changes nothing and appends nothing. Anyone
/// outside the workspace, and any emoji that is unknown or deleted, get the same `not_found`.
pub async fn rename(
    pool: &PgPool,
    actor: UserId,
    id: EmojiId,
    name: EmojiName,
) -> Result<CustomEmoji, AppError> {
    let mut conn = pool.acquire().await?;
    let row = find(&mut conn, id.as_uuid())
        .await?
        .ok_or_else(emoji_not_found)?;
    let workspace = WorkspaceId::from_uuid(row.workspace_id);
    drop(conn);
    match accounts::workspace_role(pool, workspace, actor).await? {
        None => return Err(emoji_not_found()),
        Some(role) if !role.can_manage_emoji() => return Err(manage_error()),
        Some(_) => {}
    }
    let mut tx = pool.begin().await?;
    advisory(&mut tx, format!("emoji:{workspace}")).await?;
    let row = sqlx::query_as::<_, EmojiRow>(emoji_select!("WHERE id = $1 FOR UPDATE"))
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(emoji_not_found)?;
    if row.name == name.as_str() {
        return Ok(row.to_dto());
    }
    if live_name_taken(&mut tx, workspace, name.as_str(), Some(id.as_uuid())).await? {
        return Err(name_taken());
    }
    let mut emoji = row.to_dto();
    sqlx::query("UPDATE custom_emoji SET name = $2 WHERE id = $1")
        .bind(id.as_uuid())
        .bind(name.as_str())
        .execute(&mut *tx)
        .await?;
    emoji.name = name.as_str().to_owned();
    announce(&mut tx, workspace, emoji.clone()).await?;
    tx.commit().await?;
    Ok(emoji)
}

/// Deletes a custom emoji: it stays in the workspace's list as deleted (so messages and reactions
/// that used it keep showing its name), its name is free again, it cannot be used newly, and its
/// image is removed by the cleanup job. Idempotent: a second delete, or one of an id that never
/// existed, is a quiet success. Admins and owners of its workspace only (`not_found` outside it).
pub async fn delete(pool: &PgPool, actor: UserId, id: EmojiId) -> Result<(), AppError> {
    let mut conn = pool.acquire().await?;
    let Some(row) = find(&mut conn, id.as_uuid()).await? else {
        return Ok(());
    };
    let workspace = WorkspaceId::from_uuid(row.workspace_id);
    drop(conn);
    match accounts::workspace_role(pool, workspace, actor).await? {
        None => return Err(emoji_not_found()),
        Some(role) if !role.can_manage_emoji() => return Err(manage_error()),
        Some(_) => {}
    }
    let mut tx = pool.begin().await?;
    let row: EmojiRow = sqlx::query_as(emoji_select!("WHERE id = $1 FOR UPDATE"))
        .bind(id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
    if row.deleted_at.is_some() {
        return Ok(());
    }
    sqlx::query("UPDATE custom_emoji SET deleted_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    let mut emoji = row.to_dto();
    emoji.deleted = true;
    announce(&mut tx, workspace, emoji).await?;
    tx.commit().await?;
    Ok(())
}

/// Serves a live emoji's image to a member of its workspace, streamed from the store, as an
/// immutable PNG (`ETag`, a 304 for a matching `If-None-Match`). A deleted emoji, an unknown one,
/// and one of a workspace the caller is not in all answer the same `not_found`.
pub async fn download(
    state: &AppState,
    actor: UserId,
    id: EmojiId,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let mut conn = state.pool.acquire().await?;
    let row = find(&mut conn, id.as_uuid())
        .await?
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(emoji_not_found)?;
    drop(conn);
    accounts::workspace_role(&state.pool, WorkspaceId::from_uuid(row.workspace_id), actor)
        .await?
        .ok_or_else(emoji_not_found)?;

    let etag = format!("\"{}\"", hex(&row.sha256));
    let mut response_headers = HeaderMap::new();
    let mut put = |name, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            response_headers.insert(name, value);
        }
    };
    put(ETAG, &etag);
    put(CACHE_CONTROL, "private, max-age=31536000, immutable");
    put(X_CONTENT_TYPE_OPTIONS, "nosniff");
    put(CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox");
    if headers
        .get(IF_NONE_MATCH)
        .is_some_and(|value| attachments::etag_matches(value, &etag))
    {
        return Ok((StatusCode::NOT_MODIFIED, response_headers).into_response());
    }
    let object = state
        .attachments
        .store
        .open(ObjectKey::emoji(id.as_uuid()))
        .await
        .map_err(|e| AppError::internal(format!("cannot read the image: {e}")))?;
    let Some(object) = object else {
        tracing::error!("a custom emoji's image is missing from the store");
        return Err(emoji_not_found());
    };
    let mut put = |name, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            response_headers.insert(name, value);
        }
    };
    put(CONTENT_TYPE, "image/png");
    put(CONTENT_LENGTH, &object.size.to_string());
    let body = Body::from_stream(ReaderStream::with_capacity(object.reader, 16 * 1024));
    Ok((StatusCode::OK, response_headers, body).into_response())
}

/// Removes the images of deleted emoji (the hourly cleanup job), then marks them removed. The image
/// goes before the mark, so a crash in between is retried; removing one that is already gone is not
/// an error. Returns how many were removed.
pub(crate) async fn remove_deleted_images(
    pool: &PgPool,
    store: &dyn AttachmentStore,
) -> Result<usize, AppError> {
    let mut removed = 0;
    loop {
        let doomed: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM custom_emoji WHERE deleted_at IS NOT NULL AND NOT image_removed \
             ORDER BY deleted_at, id LIMIT 500",
        )
        .fetch_all(pool)
        .await?;
        if doomed.is_empty() {
            return Ok(removed);
        }
        let mut done = Vec::with_capacity(doomed.len());
        for id in &doomed {
            match store.delete(ObjectKey::emoji(*id)).await {
                Ok(_) => done.push(*id),
                Err(_) => {
                    tracing::warn!("a deleted emoji's image could not be removed; will retry")
                }
            }
        }
        if done.is_empty() {
            return Ok(removed);
        }
        sqlx::query("UPDATE custom_emoji SET image_removed = true WHERE id = ANY($1)")
            .bind(&done)
            .execute(pool)
            .await?;
        removed += done.len();
        if done.len() < doomed.len() {
            return Ok(removed);
        }
    }
}

/// Checks the custom emoji in a message's document, in the transaction that is about to commit it:
/// every emoji it names must be live and belong to `workspace` (`invalid_request` otherwise), and
/// their rows are locked `FOR SHARE` so that a deletion cannot slip in before the commit.
///
/// `kept` are the emoji the message already had (an edit's current document): they are not checked,
/// because an edit must not be refused merely because an old emoji has been deleted since; only
/// emoji the edit adds are.
pub(crate) async fn check_document(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    document: &MessageDocument,
    kept: &[EmojiId],
) -> Result<(), AppError> {
    let wanted: Vec<Uuid> = document
        .custom_emoji()
        .into_iter()
        .filter(|id| !kept.contains(id))
        .map(|id| id.as_uuid())
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    require_live(conn, workspace, &wanted).await
}

/// A reaction with a custom emoji can be added only while it is live in the conversation's
/// workspace (`invalid_request`); standard emoji need nothing. (Withdrawing needs no check.)
pub(crate) async fn check_reaction(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    emoji: ReactionEmoji,
) -> Result<(), AppError> {
    match emoji.custom_id() {
        Some(id) => require_live(conn, workspace, &[id.as_uuid()]).await,
        None => Ok(()),
    }
}

async fn require_live(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<(), AppError> {
    let live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM custom_emoji WHERE id = ANY($1) AND workspace_id = $2 \
         AND deleted_at IS NULL ORDER BY id FOR SHARE",
    )
    .bind(ids)
    .bind(workspace.as_uuid())
    .fetch_all(conn)
    .await?;
    let mut wanted = ids.to_vec();
    wanted.sort();
    wanted.dedup();
    if live.len() == wanted.len() {
        Ok(())
    } else {
        Err(AppError::invalid(
            "an emoji used here does not exist in this workspace or has been deleted",
        ))
    }
}
